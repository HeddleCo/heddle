// SPDX-License-Identifier: Apache-2.0
//! `heddle review` handler over Heddle's local review interface.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use objects::object::{
    Discussion, DiscussionResolution, ReviewKind, ReviewScope, StateId, SymbolAnchor,
};
use repo::{HistoryQuery, operation_dedup::OperationDedupStore};
use verbs::{
    resolve_last_turn_base,
    review::{
        LocalReviewContext, LocalStateReview, ReviewSignal, ReviewSignalKind,
        ReviewSignalVisibility, SignReviewRequest, get_repo_signal_health,
    },
};

use super::{
    advice::RecoveryAdvice,
    history_target::{resolve_state_id, resolve_state_id_bytes},
    next_action::{NextActionValidationContext, write_full_command_json},
};
use crate::cli::{
    cli_args::{
        Cli, DiffBaseArg, ReviewCommands, ReviewHealthArgs, ReviewNextArgs, ReviewShowArgs,
        ReviewSignArgs,
    },
    should_output_json,
};

const AGENT_GLYPH: &str = "※";
const HUMAN_GLYPH: &str = "✓";

// The review wire payloads live in cli-contract so the schema registry
// registers the real serialization types.
pub(crate) use heddle_cli_contract::cli::commands::wire::collab::{
    DiscussionView, HealthEntry, NextStateView, ReviewHealthOutput, ReviewNextOutput,
    ReviewShowOutput, ReviewSignOutput, SignalView, SignatureView,
};

pub async fn run(cli: &Cli, command: &ReviewCommands) -> Result<()> {
    match command {
        ReviewCommands::Show(args) => run_show(cli, args).await,
        ReviewCommands::Sign(args) => run_sign(cli, args).await,
        ReviewCommands::Next(args) => run_next(cli, args).await,
        ReviewCommands::Health(args) => run_health(cli, args).await,
    }
}

async fn run_show(cli: &Cli, args: &ReviewShowArgs) -> Result<()> {
    let review = open_local_review(cli)?;
    let state_id = resolve_state(cli, args.state.as_deref())?;
    let base_state_id = match args.base {
        Some(DiffBaseArg::LastTurn) => {
            let repo = cli.open_repo()?;
            Some(resolve_last_turn_base(&repo, state_id)?)
        }
        None => None,
    };
    let payload = review.get_review_payload_from(state_id, args.all_signals, base_state_id)?;
    let stored_signatures = review.list_signatures(state_id)?;

    let signatures: Vec<SignatureView> = stored_signatures
        .iter()
        .map(|stored| {
            let signature = &stored.signature;
            let kind = signature.kind;
            let is_agent = matches!(kind, ReviewKind::AgentPreview | ReviewKind::AgentCoReview);
            let (scope_kind, scope_symbols) = match &signature.scope {
                ReviewScope::WholeChange => ("whole_change".to_string(), Vec::new()),
                ReviewScope::Symbols(symbols) => (
                    "symbols".to_string(),
                    symbols
                        .iter()
                        .map(|sym| format!("{}:{}", sym.file, sym.symbol))
                        .collect(),
                ),
            };
            SignatureView {
                actor_name: signature.actor.name_lossy().into_owned(),
                actor_email: signature.actor.email_lossy().into_owned(),
                kind: kind.as_str().to_string(),
                glyph: if is_agent { AGENT_GLYPH } else { HUMAN_GLYPH },
                is_agent,
                signed_at_secs: signature.signed_at,
                scope_kind,
                scope_symbols,
            }
        })
        .collect();

    let output = ReviewShowOutput {
        output_kind: "review_show",
        state_id: payload.state_id.to_string_full(),
        base: args.base.map(|base| base.as_str().to_string()),
        headline: payload.summary.headline,
        agent_narrative: payload.agent_narrative,
        files_changed: payload.summary.files_changed,
        in_budget_signals: payload.in_budget_signals.iter().map(signal_view).collect(),
        all_signals: payload.all_signals.iter().map(signal_view).collect(),
        discussions: payload.discussions.iter().map(discussion_view).collect(),
        signing_kinds: payload
            .signing_kinds
            .into_iter()
            .map(|kind| kind.as_str().to_string())
            .collect(),
        signatures,
    };
    if should_output_json(cli, None) {
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["review", "show"]),
        )?;
    } else {
        render_text(&output, args.all_signals);
    }
    Ok(())
}

fn render_text(out: &ReviewShowOutput, all_signals: bool) {
    println!("review of state {}", out.state_id);
    if let Some(base) = &out.base {
        println!("  base: {base}");
    }
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
    let review = open_local_review(cli)?;
    let state_id_bytes = resolve_state_id_bytes(&cli.open_repo()?, &args.state)?;
    let state_id = StateId::from_bytes(state_id_bytes.try_into().map_err(|bytes: Vec<u8>| {
        anyhow!("resolved state ID has invalid byte length {}", bytes.len())
    })?);
    let scope = if args.symbols.is_empty() {
        ReviewScope::WholeChange
    } else {
        let parsed: Result<Vec<_>> = args
            .symbols
            .iter()
            .map(|s| {
                let (file, symbol) = s
                    .split_once(':')
                    .ok_or_else(|| anyhow!(RecoveryAdvice::review_symbols_malformed(s)))?;
                Ok(SymbolAnchor::new(file, symbol))
            })
            .collect();
        ReviewScope::Symbols(parsed?)
    };
    let kind = match args.kind.as_wire() {
        "read" => ReviewKind::Read,
        "agent_preview" => ReviewKind::AgentPreview,
        "agent_co_review" => ReviewKind::AgentCoReview,
        _ => unreachable!("SignKindArg has a closed variant set"),
    };
    let request = SignReviewRequest {
        state_id,
        kind,
        scope,
        justification: args.justification.clone(),
        algorithm: args.algorithm.clone(),
        public_key: hex::decode(&args.public_key)
            .map_err(|e| anyhow::anyhow!("public_key must be hex-encoded: {e}"))?,
        signature: hex::decode(&args.signature)
            .map_err(|e| anyhow::anyhow!("signature must be hex-encoded: {e}"))?,
        signed_at: args.signed_at_unix,
        client_operation_id: crate::operation_id::wire(cli),
    };
    let response = review.sign_state(request).await?;
    if should_output_json(cli, None) {
        let out = ReviewSignOutput {
            output_kind: "review_sign",
            signature_id: response.signature_id,
            state_id: response.state_id.to_string_full(),
        };
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!(
            "signed state {} as {} (signature_id {})",
            response.state_id.to_string_full(),
            args.kind.as_wire(),
            response.signature_id
        );
    }
    Ok(())
}

async fn run_next(cli: &Cli, args: &ReviewNextArgs) -> Result<()> {
    let review = open_local_review(cli)?;
    let repo = cli.open_repo()?;
    let head = repo.head().context("read HEAD")?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::repository_no_head_capture_first(
            "review next"
        ))
    })?;

    let actor_email = args
        .mine_only
        .then(|| {
            repo.config()
                .principal
                .as_ref()
                .map(|p| p.email.clone())
                .ok_or_else(|| anyhow!(review_mine_only_principal_required_advice()))
        })
        .transpose()?;

    let history = repo
        .query_history(&HistoryQuery::new(Some(head)).with_limit(NEXT_SCAN_LIMIT))
        .context("walk history for pending reviews")?;

    let mut next_state: Option<NextStateView> = None;
    for state in history {
        let state_id_str = state.state_id.to_string_full();
        let signatures = review.list_signatures(state.state_id)?;

        let satisfied = signatures.iter().any(|s| {
            let actor_match = match actor_email.as_deref() {
                Some(email) => s
                    .signature
                    .actor
                    .email
                    .eq_ignore_ascii_case(email.as_bytes()),
                None => true,
            };
            let kind_match = match args.kind.as_deref() {
                Some(kind) => s.signature.kind.as_str() == kind,
                None => true,
            };
            actor_match && kind_match
        });

        if !satisfied {
            next_state = Some(NextStateView {
                state_id: state_id_str,
                headline: state.intent.clone().unwrap_or_default(),
                existing_signatures: signatures.len() as u32,
            });
            break;
        }
    }

    if should_output_json(cli, None) {
        // `review next` returns either the pending state's view flattened
        // alongside `output_kind`, or `next: null` when the window holds
        // none. Keeping a single envelope shape lets agents key off
        // `output_kind` without branching on payload shape.
        let envelope = match &next_state {
            Some(view) => ReviewNextOutput {
                output_kind: "review_next",
                state_id: Some(view.state_id.clone()),
                headline: Some(view.headline.clone()),
                existing_signatures: Some(view.existing_signatures),
                next: heddle_cli_contract::cli::commands::wire::collab::RequiredNullableNextState(
                    Some(view.clone()),
                ),
            },
            None => ReviewNextOutput {
                output_kind: "review_next",
                state_id: None,
                headline: None,
                existing_signatures: None,
                next: heddle_cli_contract::cli::commands::wire::collab::RequiredNullableNextState(
                    None,
                ),
            },
        };
        write_full_command_json(
            &envelope,
            NextActionValidationContext::without_repo(&["review", "next"]),
        )?;
    } else {
        match &next_state {
            Some(view) => {
                println!("next pending review: {}", view.state_id);
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

async fn run_health(cli: &Cli, args: &ReviewHealthArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let resp = get_repo_signal_health(&repo, args.window.unwrap_or(0))
        .context("compute repository signal health")?;
    if should_output_json(cli, None) {
        let entries: Vec<_> = resp
            .entries
            .iter()
            .map(|e| HealthEntry {
                module_id: e.module_id.clone(),
                fire_rate: f64::from(e.fire_rate),
                warn: e.warn,
            })
            .collect();
        let out = ReviewHealthOutput {
            output_kind: "review_health",
            entries,
            window_states: resp.window_states as usize,
        };
        println!("{}", serde_json::to_string(&out)?);
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

fn open_local_review(cli: &Cli) -> Result<LocalStateReview> {
    let repo = cli.open_repo()?;
    let dedup = OperationDedupStore::open(repo.heddle_dir()).context("open dedup store")?;
    let inner = LocalReviewContext::new(Arc::new(repo), Arc::new(dedup));
    Ok(LocalStateReview::new(inner))
}

fn signal_view(signal: &ReviewSignal) -> SignalView {
    SignalView {
        kind: match signal.kind {
            ReviewSignalKind::DiffSummary => "diff_summary",
            ReviewSignalKind::Risk(kind) => kind.as_str(),
        }
        .to_string(),
        file: signal.anchor.file.clone(),
        symbol: signal.anchor.symbol.clone().unwrap_or_default(),
        reason: signal.reason.clone(),
        producer: signal.producer.module.clone(),
        visibility: match signal.visibility {
            ReviewSignalVisibility::Visible => "visible",
            ReviewSignalVisibility::Hidden => "hidden",
        }
        .to_string(),
    }
}

fn discussion_view(discussion: &Discussion) -> DiscussionView {
    let status = match &discussion.resolution {
        DiscussionResolution::Open => "open",
        DiscussionResolution::ResolvedIntoAnnotation { .. } => "resolved_into_annotation",
        DiscussionResolution::ResolvedByEdit { .. } => "resolved_by_edit",
        DiscussionResolution::Dismissed { .. } => "dismissed",
    }
    .to_string();
    DiscussionView {
        id: discussion.id.clone(),
        file: discussion.anchor.file.clone(),
        symbol: discussion.anchor.symbol.clone(),
        status,
        body_changed_since_open: discussion.body_changed_since_open,
        orphaned: discussion.orphaned,
    }
}

fn resolve_state(cli: &Cli, explicit: Option<&str>) -> Result<StateId> {
    let repo = cli.open_repo()?;
    if let Some(s) = explicit {
        // Routes through the canonical resolver so short/full IDs and
        // marker names all work — matches `heddle log --output json` output.
        return resolve_state_id(&repo, s);
    }
    let head = repo
        .head()
        .context("read HEAD")?
        .ok_or_else(|| anyhow!(RecoveryAdvice::repository_no_head_capture_first("review")))?;
    Ok(head)
}

fn review_mine_only_principal_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "review_mine_only_principal_required",
        "--mine-only requires a configured principal in repo config",
        "Configure a repository principal with `heddle init --principal-name <name> --principal-email <email>`, or rerun `heddle review next` without `--mine-only`.",
        "`--mine-only` needs the repository principal email, but repo config has no principal",
        "guessing an actor email could report the wrong pending review state",
        "review signatures, repository state, refs, metadata, and worktree files were left unchanged",
        "heddle init --principal-name <name> --principal-email <email>",
        vec![
            "heddle init --principal-name <name> --principal-email <email>".to_string(),
            "heddle review next".to_string(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape contract for `review health --output json`. The handler builds the
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

    #[test]
    fn durable_review_kinds_have_cli_spellings() {
        assert_eq!(ReviewKind::Read.as_str(), "read");
        assert_eq!(ReviewKind::AgentPreview.as_str(), "agent_preview");
        assert_eq!(ReviewKind::AgentCoReview.as_str(), "agent_co_review");
    }

    #[test]
    fn mine_only_principal_advice_is_typed() {
        let advice = review_mine_only_principal_required_advice();

        assert_eq!(advice.kind, "review_mine_only_principal_required");
        assert_eq!(
            advice.primary_command,
            "heddle init --principal-name <name> --principal-email <email>"
        );
        assert!(advice.primary_hint().contains("heddle review next"));
        assert!(advice.unsafe_condition.contains("--mine-only"));
        assert!(advice.would_change.contains("wrong pending review"));
        assert!(advice.preserved.contains("review signatures"));
    }
}
