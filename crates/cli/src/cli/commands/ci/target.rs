// SPDX-License-Identifier: Apache-2.0
//! Exact working-tree and named-state evaluation targets.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use objects::{
    object::{ContentHash, State},
    store::ObjectStore,
};
use repo::{AudienceTier, CheckoutMaterialization, Repository};

pub(crate) struct EvaluationTarget {
    pub(crate) workdir: PathBuf,
    pub(crate) state: State,
    pub(crate) tree_digest: ContentHash,
    worktree_fingerprint: ContentHash,
    checkout: Option<tempfile::TempDir>,
}

impl EvaluationTarget {
    pub(crate) fn prepare(repo: &Repository, state: Option<&str>, record: bool) -> Result<Self> {
        match state {
            Some(spec) => Self::from_state(repo, spec),
            None => Self::from_worktree(repo, record),
        }
    }

    fn from_worktree(repo: &Repository, record: bool) -> Result<Self> {
        let mut state = repo
            .current_state()?
            .context("local CI needs a current state; capture the working tree first")?;
        let worktree_fingerprint = repo.build_tree(repo.root())?.hash();
        let tree_digest = if record {
            let tree = repo.require_tree(&state.tree)?;
            if !repo.compare_worktree_cached(&tree)?.is_clean() {
                bail!(
                    "recording requires the exact captured State tree; capture the working tree or select --state"
                );
            }
            state.tree
        } else {
            state.tree = worktree_fingerprint;
            worktree_fingerprint
        };
        Ok(Self {
            workdir: repo.root().to_path_buf(),
            state,
            tree_digest,
            worktree_fingerprint,
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
            CheckoutMaterialization::Withheld { tier } => {
                repo.clear_materialized_root_records(checkout.path())?;
                bail!("state {spec:?} is withheld at visibility tier {tier:?}");
            }
        };
        if tree.hash() != state.tree {
            repo.clear_materialized_root_records(checkout.path())?;
            bail!("materialized tree for state {spec:?} does not match its recorded digest");
        }
        Ok(Self {
            workdir: checkout.path().to_path_buf(),
            tree_digest: state.tree,
            state,
            worktree_fingerprint: repo.build_tree(checkout.path())?.hash(),
            checkout: Some(checkout),
        })
    }

    pub(crate) fn ensure_unchanged(&self, repo: &Repository) -> Result<()> {
        if repo.build_tree(&self.workdir)?.hash() != self.worktree_fingerprint {
            bail!("working tree changed while CI checks ran; refusing to sign a stale tree digest");
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
