// SPDX-License-Identifier: Apache-2.0
//! Exact working-tree and named-state evaluation targets.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use objects::{
    object::{ContentHash, State, VisibilityTier},
    store::ObjectStore,
};
use repo::{AudienceTier, CheckoutMaterialization, Repository};

use crate::cli::commands::RecoveryAdvice;

pub(crate) struct EvaluationTarget {
    pub(crate) workdir: PathBuf,
    pub(crate) state: State,
    pub(crate) tree_digest: ContentHash,
    kind: TargetKind,
    checkout: Option<tempfile::TempDir>,
}

#[derive(Clone, Copy)]
enum TargetKind {
    Worktree,
    State,
}

impl EvaluationTarget {
    pub(crate) fn prepare(repo: &Repository, state: Option<&str>) -> Result<Self> {
        match state {
            Some(spec) => Self::from_state(repo, spec),
            None => Self::from_worktree(repo),
        }
    }

    fn from_worktree(repo: &Repository) -> Result<Self> {
        let mut state = repo
            .current_state()?
            .context("local CI needs a current state; capture the working tree first")?;
        let tree_digest = repo.build_tree(repo.root())?.hash();
        state.tree = tree_digest;
        Ok(Self {
            workdir: repo.root().to_path_buf(),
            state,
            tree_digest,
            kind: TargetKind::Worktree,
            checkout: None,
        })
    }

    fn from_state(repo: &Repository, spec: &str) -> Result<Self> {
        let state_id = repo
            .resolve_state(spec)?
            .with_context(|| format!("state {spec:?} was not found"))?;
        let state = repo
            .store()
            .get_state(&state_id)?
            .with_context(|| format!("state object {state_id} was not found"))?;
        let checkout = tempfile::Builder::new()
            .prefix("heddle-ci-state-")
            .tempdir()
            .context("create local CI state checkout")?;
        let materialized =
            repo.checkout_state_gated(&state_id, &state, checkout.path(), &AudienceTier::Internal)?;
        let tree = match materialized {
            CheckoutMaterialization::Materialized { tree } => tree,
            CheckoutMaterialization::Filtered { .. } => {
                repo.clear_materialized_root_records(checkout.path())?;
                return Err(anyhow::anyhow!(ci_filtered_public_tip_advice(spec)));
            }
            CheckoutMaterialization::Withheld { tier } => {
                repo.clear_materialized_root_records(checkout.path())?;
                return Err(anyhow::anyhow!(ci_withheld_state_advice(spec, &tier)));
            }
        };
        if tree.hash() != state.tree {
            repo.clear_materialized_root_records(checkout.path())?;
            return Err(anyhow::anyhow!(ci_tree_digest_mismatch_advice(spec)));
        }
        Ok(Self {
            workdir: checkout.path().to_path_buf(),
            tree_digest: state.tree,
            state,
            kind: TargetKind::State,
            checkout: Some(checkout),
        })
    }

    pub(crate) fn ensure_unchanged(&self, repo: &Repository) -> Result<()> {
        if matches!(self.kind, TargetKind::Worktree) {
            let after = repo.build_tree(repo.root())?.hash();
            if after != self.tree_digest {
                bail!(
                    "working tree changed while CI checks ran; refusing to sign a stale tree digest"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup(&mut self, repo: &Repository) -> Result<()> {
        if let Some(checkout) = self.checkout.take() {
            repo.clear_materialized_root_records(checkout.path())?;
        }
        Ok(())
    }
}

const CI_RUN_LOCAL: &str = "heddle ci run --local";

fn ci_filtered_public_tip_advice(spec: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "ci_filtered_public_tip",
        format!("state {spec:?} is a filtered public tip; CI refuses to sign a partial tree"),
        format!("Run `{CI_RUN_LOCAL}` on a fully visible state, or inspect the tip with `heddle visibility show {spec}`."),
        format!("state {spec:?} still names private-ancestor blobs that Internal CI cannot serve"),
        "signing a filtered tree would attest a digest that is not the recorded tip",
        "the temporary CI checkout was discarded; repository state was left unchanged",
        CI_RUN_LOCAL,
        vec![
            CI_RUN_LOCAL.to_string(),
            format!("heddle visibility show {spec}"),
        ],
    )
}

fn ci_withheld_state_advice(spec: &str, tier: &VisibilityTier) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "ci_withheld_state",
        format!(
            "state {spec:?} is withheld at visibility tier {}",
            tier.as_str()
        ),
        format!("Pick a state Internal can see, or inspect it with `heddle visibility show {spec}`."),
        format!(
            "state {spec:?} is {} to the Internal CI audience",
            tier.as_str()
        ),
        "signing a withheld stub would attest a tree CI did not materialize",
        "the temporary CI checkout was discarded; repository state was left unchanged",
        CI_RUN_LOCAL,
        vec![
            CI_RUN_LOCAL.to_string(),
            format!("heddle visibility show {spec}"),
        ],
    )
}

fn ci_tree_digest_mismatch_advice(spec: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "ci_tree_digest_mismatch",
        format!("materialized tree for state {spec:?} does not match its recorded digest"),
        "Re-run `heddle doctor`, then `heddle ci run --local` on a state whose tree is intact.",
        format!("checkout of {spec:?} produced a tree hash that is not the state's recorded digest"),
        "signing a mismatched tree would attest bytes the named state does not record",
        "the temporary CI checkout was discarded; repository state was left unchanged",
        CI_RUN_LOCAL,
        vec!["heddle doctor".to_string(), CI_RUN_LOCAL.to_string()],
    )
}
