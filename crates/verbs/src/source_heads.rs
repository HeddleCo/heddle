// SPDX-License-Identifier: Apache-2.0
//! Concurrent source heads on one Thread: the report clone, pull, status and
//! `resolve --heads` share, head selection, and the pick or merge that
//! resolves them.
//!
//! The default head a clone or pull materializes is chosen by
//! [`repo::thread_replication::source_heads::default_source_head`]; this
//! module never re-derives it.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use objects::{
    HeddleError, RecoveryDetails,
    object::{Attribution, Blob, StateId, ThreadName},
    store::ObjectStore,
};
use refs::Head;
use repo::{CommitGraphIndex, Repository, thread_replication::source_heads::DefaultHeadRule};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    merge::{
        ConflictLabels, MergeAttemptPlan, MergePlan, MergeRelationKind, apply_merged_tree,
        ensure_worktree_clean,
    },
    resolve::ClaimedProducerReport,
    status::next_action::heddle_action,
};

/// Lists every head with its claimed producer and the pick / merge commands.
pub const SOURCE_HEADS_ACTION: &str = "heddle resolve --heads";

/// A Thread whose source has several concurrent heads. Every head is listed;
/// choosing one to check out never discards the others.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SourceHeadsReport {
    pub thread: String,
    /// Full State ID of the local Thread tip.
    pub current: Option<String>,
    /// Rule a clone or pull used to choose `current`. Absent outside a
    /// transfer. One of `local_tip`, `local_lineage_greatest_state_id`,
    /// `greatest_state_id`.
    pub selected_by: Option<String>,
    /// Every concurrent head, greatest State ID first.
    pub heads: Vec<SourceHeadReport>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SourceHeadReport {
    pub state: String,
    /// This head is the local Thread tip.
    pub current: bool,
    /// Attribution recorded in the head's State, not an authenticated actor.
    pub producer: ClaimedProducerReport,
    pub intent: Option<String>,
    /// Resolve the Thread to exactly this head's tree.
    pub pick_action: String,
    /// Three-way merge this head into the local tip. Absent for the tip.
    pub merge_action: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceHeadResolutionMode {
    /// The Thread takes exactly the selected head's tree.
    Pick,
    /// The selected head is three-way merged into the local tip.
    Merge,
}

/// Outcome of `heddle resolve --pick` / `--merge`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SourceHeadResolutionReport {
    pub mode: SourceHeadResolutionMode,
    pub thread: String,
    pub selected: String,
    /// The resolving capture. Absent while a merge waits on conflicts.
    pub state: Option<String>,
    /// The resolving capture's parents: every head it retires.
    pub parents: Vec<String>,
}

impl SourceHeadsReport {
    /// The blocker line status and ready show while the heads are unresolved.
    pub fn blocker(&self) -> String {
        format!(
            "thread '{}' has {} unresolved alternative source heads; pick or merge one",
            self.thread,
            self.heads.len()
        )
    }
}

fn thread_error(error: repo::thread_replication::Error) -> HeddleError {
    HeddleError::InvalidObject(error.to_string())
}

/// Report the concurrent heads of local Thread `thread`, or `None` when it
/// has at most one head (or no native identity).
pub fn source_heads_report(
    repo: &Repository,
    thread: &str,
    selected_by: Option<DefaultHeadRule>,
) -> objects::error::Result<Option<SourceHeadsReport>> {
    let Some(heads) = repo.native_source_heads(thread).map_err(thread_error)? else {
        return Ok(None);
    };
    if heads.len() < 2 {
        return Ok(None);
    }
    let tip = repo.refs().get_thread(&ThreadName::new(thread))?;
    let mut entries = Vec::with_capacity(heads.len());
    for head in heads.iter().rev() {
        let state = repo
            .store()
            .get_state(head)?
            .ok_or(HeddleError::StateNotFound(*head))?;
        let full = head.to_string_full();
        let current = tip == Some(*head);
        entries.push(SourceHeadReport {
            pick_action: heddle_action(["resolve", "--pick", full.as_str()]),
            merge_action: (!current).then(|| heddle_action(["resolve", "--merge", full.as_str()])),
            state: full,
            current,
            producer: (&state.attribution).into(),
            intent: state.intent.clone(),
        });
    }
    Ok(Some(SourceHeadsReport {
        thread: thread.to_string(),
        current: tip.map(|tip| tip.to_string_full()),
        selected_by: selected_by
            .filter(|rule| *rule != DefaultHeadRule::Sole)
            .map(|rule| rule.as_str().to_string()),
        heads: entries,
    }))
}

/// The rule a successful pull applied. A pull only succeeds when the chosen
/// head is the local tip or fast-forwards it, so the facts determine the
/// rule [`repo::thread_replication::source_heads::default_source_head`] used.
pub fn pull_selection_rule(
    repo: &Repository,
    local_tip: Option<StateId>,
    chosen: StateId,
) -> objects::error::Result<DefaultHeadRule> {
    let Some(tip) = local_tip else {
        return Ok(DefaultHeadRule::GreatestStateId);
    };
    if tip == chosen {
        return Ok(DefaultHeadRule::LocalTip);
    }
    let mut graph = CommitGraphIndex::new(repo);
    Ok(
        if graph
            .is_ancestor(&tip, &chosen)
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?
        {
            DefaultHeadRule::LocalLineage
        } else {
            DefaultHeadRule::GreatestStateId
        },
    )
}

/// Match `selector` (a full `hs-…` State ID, or a unique prefix with or
/// without `hs-`) against the Thread's heads only.
pub fn resolve_head_selector(
    heads: &BTreeSet<StateId>,
    thread: &str,
    selector: &str,
) -> std::result::Result<StateId, HeddleError> {
    let wanted = selector.trim().to_ascii_lowercase();
    let wanted = wanted.strip_prefix("hs-").unwrap_or(&wanted);
    let matches = if wanted.is_empty() {
        Vec::new()
    } else {
        heads
            .iter()
            .copied()
            .filter(|head| {
                let full = head.to_string_full();
                full.strip_prefix("hs-")
                    .unwrap_or(&full)
                    .starts_with(wanted)
            })
            .collect::<Vec<_>>()
    };
    match matches.as_slice() {
        [head] => Ok(*head),
        [] => Err(HeddleError::recovery(
            RecoveryDetails::safety_refusal(
                "unknown_source_head",
                format!("'{selector}' is not a source head of thread '{thread}'"),
                "List the Thread's heads with `heddle resolve --heads`, then select one by its State ID.",
                format!("no current source head of '{thread}' matches '{selector}'"),
                "resolving to a State that is not a current head would discard every real alternative",
                "repository state, refs and worktree files were left unchanged",
            )
            .with_recovery_commands(vec![SOURCE_HEADS_ACTION.to_string()]),
        )),
        _ => Err(HeddleError::recovery(
            RecoveryDetails::safety_refusal(
                "ambiguous_source_head",
                format!(
                    "'{selector}' matches {} source heads of thread '{thread}'",
                    matches.len()
                ),
                "Use more of the State ID shown by `heddle resolve --heads`.",
                format!("'{selector}' is a prefix of several heads"),
                "picking one of several matches would be a guess",
                "repository state, refs and worktree files were left unchanged",
            )
            .with_recovery_commands(vec![SOURCE_HEADS_ACTION.to_string()]),
        )),
    }
}

/// The Thread HEAD is attached to. Source heads belong to one Thread, so a
/// detached HEAD has none to list or resolve.
pub fn attached_thread(repo: &Repository) -> Result<String> {
    match repo.head_ref()? {
        Head::Attached { thread } => Ok(thread.to_string()),
        Head::Detached { .. } => Err(anyhow!(HeddleError::recovery(
            RecoveryDetails::safety_refusal(
                "source_head_resolution_detached",
                "Source heads belong to a checked-out Thread; HEAD is detached",
                "Switch to the Thread with `heddle thread switch <name>`, then retry.",
                "HEAD is detached, so no Thread's heads can be listed or resolved",
                "a resolution capture must advance one Thread",
                "repository state, refs and worktree files were left unchanged",
            )
            .with_recovery_commands(vec!["heddle status".to_string()])
        ))),
    }
}

/// The attached Thread and its heads, after refusing every state a pick or
/// merge must not start from.
pub struct SourceHeadSelection {
    pub thread: String,
    pub tip: StateId,
    pub heads: BTreeSet<StateId>,
    pub selected: StateId,
}

/// Preflight shared by pick and merge: an attached Thread with several heads,
/// no merge in progress, a clean worktree, and a selector naming one head.
pub fn select_source_head(repo: &Repository, selector: &str) -> Result<SourceHeadSelection> {
    if repo.merge_state_manager().is_merge_in_progress() {
        return Err(anyhow!(crate::merge::merge_already_in_progress_error()));
    }
    let thread = attached_thread(repo)?;
    let heads = repo
        .native_source_heads(&thread)
        .map_err(thread_error)?
        .unwrap_or_default();
    if heads.len() < 2 {
        return Err(anyhow!(HeddleError::recovery(
            RecoveryDetails::safety_refusal(
                "no_alternative_source_heads",
                format!("Thread '{thread}' has no alternative source heads"),
                "Inspect the Thread with `heddle status`.",
                format!("thread '{thread}' has {} source head(s)", heads.len()),
                "there is nothing to pick or merge",
                "repository state, refs and worktree files were left unchanged",
            )
            .with_recovery_commands(vec!["heddle status".to_string()])
        )));
    }
    let selected = resolve_head_selector(&heads, &thread, selector)?;
    ensure_worktree_clean(repo, "resolve source heads")?;
    let tip = repo
        .refs()
        .get_thread(&ThreadName::new(&thread))?
        .ok_or(HeddleError::NotFound(format!("thread '{thread}' tip")))?;
    Ok(SourceHeadSelection {
        thread,
        tip,
        heads,
        selected,
    })
}

/// Resolve every head to `selection.selected`: one capture whose tree is that
/// head's tree and whose parents name every head.
pub fn pick_source_head(
    repo: &Repository,
    selection: &SourceHeadSelection,
    attribution: Attribution,
) -> Result<StateId> {
    let state = repo
        .pick_native_source_head(
            &selection.thread,
            selection.selected,
            attribution,
            format!("Pick source head {}", selection.selected.short()),
        )
        .map_err(thread_error)?;
    Ok(state.id())
}

pub enum MergeSourceHeadOutcome {
    /// A merge capture whose parents are the tip and the selected head.
    Merged(StateId),
    /// Conflicts are in the worktree and in merge state; `heddle resolve`
    /// then `heddle continue` finish the merge.
    Conflicted(Vec<String>),
}

/// Three-way merge `selection.selected` into the Thread tip.
pub fn merge_source_head(
    repo: &Repository,
    selection: &SourceHeadSelection,
    attribution: Attribution,
) -> Result<MergeSourceHeadOutcome> {
    let SourceHeadSelection {
        thread,
        tip,
        selected,
        ..
    } = selection;
    if tip == selected {
        return Err(anyhow!(HeddleError::recovery(
            RecoveryDetails::safety_refusal(
                "source_head_already_current",
                format!(
                    "{} is already the tip of thread '{thread}'",
                    selected.short()
                ),
                "Merge a different head, or pick this one with `heddle resolve --pick`.",
                "the selected head is the local tip",
                "merging a head into itself changes nothing",
                "repository state, refs and worktree files were left unchanged",
            )
            .with_recovery_commands(vec![heddle_action([
                "resolve",
                "--pick",
                selected.to_string_full().as_str(),
            ])])
        )));
    }
    // A hosted clone carries each head's content, not its ancestors'; clone
    // and pull fetch the merge base when Weft publishes it.
    let pick_instead = || {
        vec![heddle_action([
            "resolve",
            "--pick",
            selected.to_string_full().as_str(),
        ])]
    };
    if let Some(base) = CommitGraphIndex::new(repo).find_merge_base(tip, selected)? {
        let present = match repo.store().get_state(&base)? {
            Some(state) => repo.store().get_tree(&state.tree)?.is_some(),
            None => false,
        };
        if !present {
            return Err(anyhow!(HeddleError::recovery(
                RecoveryDetails::safety_refusal(
                    "source_head_merge_base_unavailable",
                    format!(
                        "The merge base {} of the tip and {} is not available locally",
                        base.short(),
                        selected.short()
                    ),
                    "Pick one head with `heddle resolve --pick`, which needs no merge base, or pull again once the base is published.",
                    "a three-way merge needs the common ancestor's content",
                    "merging against a missing base would treat it as empty and could drop files",
                    "repository state, refs and worktree files were left unchanged",
                )
                .with_recovery_commands(pick_instead())
            )));
        }
    }
    let current_label = format!("CURRENT ({thread})");
    let incoming_label = format!("INCOMING ({})", selected.short());
    let mut graph = CommitGraphIndex::new(repo);
    let plan = MergePlan::for_merge_command(
        repo,
        &mut graph,
        tip,
        selected,
        ConflictLabels {
            current: &current_label,
            incoming: &incoming_label,
            strategy: MergeAttemptPlan::decide(false).strategy(),
        },
    )?;
    let relation = plan.relation();
    let base = relation.merge_base_id();
    let result = match relation.kind() {
        MergeRelationKind::CleanApply
        | MergeRelationKind::Conflicted
        | MergeRelationKind::AlreadyIntegrated => plan
            .merge_result()
            .ok_or_else(|| anyhow!("merge plan for a source head has no merge result"))?,
        MergeRelationKind::AlreadyUpToDate | MergeRelationKind::FastForward => {
            // Concurrent heads never contain one another; the local tip is
            // behind every head only before a pull finishes. A pick names
            // every head as a parent and so still resolves the Thread.
            return Err(anyhow!(HeddleError::recovery(
                RecoveryDetails::safety_refusal(
                    "source_head_not_concurrent",
                    format!(
                        "{} and the tip of thread '{thread}' are not concurrent",
                        selected.short()
                    ),
                    "Pick the head instead with `heddle resolve --pick`.",
                    "one side already contains the other, so there is nothing to merge",
                    "a merge capture here would not name every head",
                    "repository state, refs and worktree files were left unchanged",
                )
                .with_recovery_commands(pick_instead())
            )));
        }
    };
    apply_merged_tree(repo, &result.tree)?;
    if result.conflicts.is_empty() {
        let state = repo.snapshot_merge_with_attribution(
            selected,
            Some(format!("Merge source head {}", selected.short())),
            None,
            attribution,
            base,
            false,
        )?;
        return Ok(MergeSourceHeadOutcome::Merged(state.id()));
    }
    let structured = plan
        .structured_conflicts()
        .map(|payload| -> Result<_> { Ok(repo.store().put_blob(&Blob::new(payload.encode()?))?) })
        .transpose()?;
    repo.merge_state_manager().start(
        *tip,
        *selected,
        base,
        result.conflicts.clone(),
        structured,
    )?;
    Ok(MergeSourceHeadOutcome::Conflicted(result.conflicts.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(error: HeddleError) -> &'static str {
        match error {
            HeddleError::Recovery(details) => details.kind,
            other => panic!("expected a typed refusal, got {other}"),
        }
    }

    #[test]
    fn head_selector_matches_one_head_by_unique_prefix_only() {
        let low = StateId::from_bytes([0; 32]);
        let mut near = [0; 32];
        near[31] = 1;
        let near = StateId::from_bytes(near);
        let high = StateId::from_bytes([0xff; 32]);
        let heads = BTreeSet::from([low, near, high]);
        let full = high.to_string_full();
        for selector in [
            full.clone(),
            full.to_ascii_uppercase(),
            full.trim_start_matches("hs-").to_string(),
            full[..10].to_string(),
        ] {
            assert_eq!(
                resolve_head_selector(&heads, "main", &selector).expect("unique head"),
                high
            );
        }
        let shared = &low.to_string_full()[..20];
        assert_eq!(
            kind(resolve_head_selector(&heads, "main", shared).expect_err("two heads")),
            "ambiguous_source_head"
        );
        for selector in ["", "hs-", "hs-yyyy", "not-a-head"] {
            assert_eq!(
                kind(resolve_head_selector(&heads, "main", selector).expect_err("no head")),
                "unknown_source_head"
            );
        }
    }
}
