// SPDX-License-Identifier: Apache-2.0
//! Who is acting on this repository: principal resolution (environment →
//! config → Git identity), attribution, and the synthetic seed identity
//! stamped into genesis states.

use std::path::Path;

use objects::{
    error::Result,
    object::{Attribution, Principal, State},
};

use super::{Repository, RepositoryCapability, open_git_repository_at_root};

impl Repository {
    pub fn get_principal(&self) -> Result<Principal> {
        if let Some(principal) = Principal::from_env() {
            return Ok(principal);
        }

        if let Some(config) = &self.config.principal {
            return Ok(Principal::new(&config.name, &config.email));
        }

        if self.capability() == RepositoryCapability::GitOverlay
            && let Some(principal) = git_config_principal(&self.root)
        {
            return Ok(principal);
        }

        if let Some(principal) = self.shared_checkout_parent_git_principal() {
            return Ok(principal);
        }

        Ok(Principal::new("Unknown", "unknown@example.com"))
    }

    fn shared_checkout_parent_git_principal(&self) -> Option<Principal> {
        let local_heddle_dir = self.root.join(".heddle");
        if local_heddle_dir == self.heddle_dir || !local_heddle_dir.join("objectstore").is_file() {
            return None;
        }
        let parent_root = self.heddle_dir.parent()?;
        if parent_root == self.root {
            return None;
        }
        git_config_principal(parent_root)
    }

    pub fn get_attribution(&self) -> Result<Attribution> {
        let principal = self.get_principal()?;

        if let Some(agent) = self.resolve_agent() {
            Ok(Attribution::with_agent(principal, agent))
        } else {
            Ok(Attribution::human(principal))
        }
    }
}

/// Stable system principal stamped into the synthetic seed state created
/// at `heddle init` time, before any user principal is known. Kept
/// distinct from the `Unknown <unknown@example.com>` fallback so the
/// genesis state is never confused with an unattributed user state.
pub(crate) fn seed_principal() -> Principal {
    Principal::new("Heddle", "init@heddle")
}

/// True if `state` is the synthetic empty-tree genesis stamped by
/// [`Repository::seed_default_thread`]. These states are filtered from
/// user-facing log walks: they have no parents, no intent, and the
/// system seed principal — they represent pre-history, not user work.
pub fn is_synthetic_root(state: &State) -> bool {
    state.parents.is_empty()
        && state.intent.is_none()
        && state.attribution.principal == seed_principal()
        && state.attribution.agent.is_none()
}

fn git_config_principal(root: &Path) -> Option<Principal> {
    let git_repo = open_git_repository_at_root(root).ok().flatten()?;
    let config = git_repo.config_snapshot().ok()?;
    let name = config.get("user", None, "name")?.to_string();
    let email = config.get("user", None, "email")?.to_string();
    if name.trim().is_empty() || email.trim().is_empty() {
        return None;
    }
    Some(Principal::new(&name, &email))
}
