// SPDX-License-Identifier: Apache-2.0
//! Exact working-tree and named-state evaluation targets.

use std::path::{Path, PathBuf};

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

pub(crate) fn config_path(repo: &Repository, explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.heddle_dir().join("ci.toml"))
}
