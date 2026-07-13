// SPDX-License-Identifier: Apache-2.0
//! State resolution helpers for the Repository.

use objects::{
    object::{Agent, StateId},
    store::ObjectStore,
};

use super::{HeddleError, Repository, Result};

impl Repository {
    /// Resolve a state specifier (HEAD, thread, marker, full/short ID, HEAD~N).
    pub fn resolve_state(&self, spec: &str) -> Result<Option<StateId>> {
        if let Some(steps) = parse_head_steps(spec) {
            return resolve_head_steps(self, steps);
        }

        if let Some(id) = self.refs.resolve(spec)? {
            return Ok(Some(id));
        }

        if self.capability() == super::RepositoryCapability::GitOverlay {
            if let Some(id) = self.git_overlay_mapped_change_for_branch(spec)? {
                return Ok(Some(id));
            }
            if let Some(id) = self.git_overlay_mapped_change_for_tag(spec)? {
                return Ok(Some(id));
            }
        }

        resolve_short_state_id(self, spec)
    }

    pub fn resolve_agent(&self) -> Option<Agent> {
        let provider = std::env::var("HEDDLE_AGENT_PROVIDER")
            .ok()
            .or_else(|| self.config.agent.provider.clone());
        let model = std::env::var("HEDDLE_AGENT_MODEL")
            .ok()
            .or_else(|| self.config.agent.model.clone());
        let session_id = std::env::var("HEDDLE_SESSION_ID").ok();
        let segment_id = std::env::var("HEDDLE_SESSION_SEGMENT").ok();
        let policy_id = std::env::var("HEDDLE_AGENT_POLICY")
            .ok()
            .or_else(|| self.config.policies.default_policy.clone());

        match (provider, model) {
            (Some(provider), Some(model)) => {
                let mut agent = Agent::new(provider, model);
                if let (Some(sid), Some(segid)) = (session_id, segment_id) {
                    agent = agent.with_session(sid, segid);
                }
                if let Some(policy_id) = policy_id {
                    agent = agent.with_policy(policy_id);
                }
                Some(agent)
            }
            _ => None,
        }
    }
}

fn parse_head_steps(spec: &str) -> Option<usize> {
    if spec == "HEAD" || spec == "@" {
        return Some(0);
    }

    let rest = spec
        .strip_prefix("HEAD~")
        .or_else(|| spec.strip_prefix("@~"))?;
    if rest.is_empty() {
        return None;
    }
    rest.parse::<usize>().ok()
}

fn resolve_head_steps(repo: &Repository, steps: usize) -> Result<Option<StateId>> {
    let mut current = repo.head()?;
    if steps == 0 {
        return Ok(current);
    }

    for _ in 0..steps {
        let Some(id) = current else {
            return Ok(None);
        };
        let state = repo.store.get_state(&id)?;
        let Some(state) = state else {
            return Ok(None);
        };
        current = state.first_parent().copied();
    }

    Ok(current)
}

fn resolve_short_state_id(repo: &Repository, spec: &str) -> Result<Option<StateId>> {
    let prefix = spec.strip_prefix("hs-").unwrap_or(spec).to_lowercase();
    if prefix.len() < 4 {
        return Ok(None);
    }

    let mut matches = Vec::new();
    for id in repo.store.list_states()? {
        let full = id.to_string_full();
        let full_norm = full.strip_prefix("hs-").unwrap_or(&full).to_lowercase();
        if full_norm.starts_with(&prefix) {
            matches.push(id);
        }
    }

    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => {
            // Render up to 5 candidates (full form) so callers can
            // disambiguate without re-listing states.
            let mut shown: Vec<String> = matches
                .iter()
                .take(5)
                .map(|id| id.to_string_full())
                .collect();
            if matches.len() > shown.len() {
                shown.push(format!("... ({} more)", matches.len() - shown.len()));
            }
            Err(HeddleError::Conflict(format!(
                "ambiguous state ID prefix '{}' matches: {}",
                spec,
                shown.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::{
        object::{MarkerName, StateId},
        store::ObjectStore,
    };
    use tempfile::TempDir;

    use crate::{HeddleError, Repository};

    /// Init a repo and capture two snapshots so we have a real history
    /// to resolve against.
    fn repo_with_two_states() -> (TempDir, Repository, StateId, StateId) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        fs::write(temp.path().join("a.txt"), "a").unwrap();
        let s1 = repo.snapshot(Some("first".into()), None).unwrap();
        fs::write(temp.path().join("b.txt"), "b").unwrap();
        let s2 = repo.snapshot(Some("second".into()), None).unwrap();
        (temp, repo, s1.id(), s2.id())
    }

    #[test]
    fn resolve_state_accepts_full_id() {
        let (_t, repo, s1, _) = repo_with_two_states();
        let full = s1.to_string_full();
        let resolved = repo.resolve_state(&full).unwrap();
        assert_eq!(resolved, Some(s1));
    }

    #[test]
    fn resolve_state_accepts_short_prefix() {
        let (_t, repo, s1, _) = repo_with_two_states();
        // `short()` is the form `heddle log --json` prints.
        let short = s1.short();
        let resolved = repo.resolve_state(&short).unwrap();
        assert_eq!(resolved, Some(s1));
    }

    #[test]
    fn resolve_state_accepts_short_prefix_without_hd_prefix() {
        // The resolver also tolerates the bare encoding without `hs-`.
        let (_t, repo, s1, _) = repo_with_two_states();
        let short = s1.short();
        let bare = short.strip_prefix("hs-").unwrap();
        let resolved = repo.resolve_state(bare).unwrap();
        assert_eq!(resolved, Some(s1));
    }

    #[test]
    fn resolve_state_returns_none_for_unknown_id() {
        let (_t, repo, _, _) = repo_with_two_states();
        // Length>=4 so we exercise the index path, not the
        // too-short-prefix shortcut.
        assert_eq!(repo.resolve_state("hs-zzzz").unwrap(), None);
    }

    #[test]
    fn resolve_state_returns_none_for_too_short_prefix() {
        let (_t, repo, _, _) = repo_with_two_states();
        assert_eq!(repo.resolve_state("hd").unwrap(), None);
    }

    #[test]
    fn resolve_state_accepts_marker_name() {
        let (_t, repo, s1, _) = repo_with_two_states();
        repo.refs()
            .create_marker(&MarkerName::new("milestone-1"), &s1)
            .unwrap();
        let resolved = repo.resolve_state("milestone-1").unwrap();
        assert_eq!(resolved, Some(s1));
    }

    #[test]
    fn resolve_state_accepts_head() {
        let (_t, repo, _, s2) = repo_with_two_states();
        let resolved = repo.resolve_state("HEAD").unwrap();
        assert_eq!(resolved, Some(s2));
    }

    #[test]
    fn resolve_state_accepts_head_steps() {
        let (_t, repo, s1, s2) = repo_with_two_states();
        assert_eq!(repo.resolve_state("HEAD").unwrap(), Some(s2));
        assert_eq!(repo.resolve_state("HEAD~1").unwrap(), Some(s1));
    }

    /// Ambiguous-prefix detection: synthesize two states whose full
    /// IDs share a common prefix by writing them straight to the store
    /// at hand-picked IDs. Going through the snapshot path can't
    /// reliably produce a collision because state IDs are random.
    #[test]
    fn resolve_state_errors_on_ambiguous_prefix() {
        use objects::object::{Attribution, State};
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        let head = repo.head().unwrap().unwrap();
        let head_state = repo.store().get_state(&head).unwrap().unwrap();
        let principal = repo.get_principal().unwrap();
        let state_a = State::new(
            head_state.tree,
            vec![head],
            Attribution::human(principal.clone()),
        );
        let state_b = State::new(head_state.tree, vec![head], Attribution::human(principal));
        let id_a = state_a.id();
        let id_b = state_b.id();
        repo.store().put_state(&state_a).unwrap();
        repo.store().put_state(&state_b).unwrap();

        // Sanity: both states must be visible to `list_states` for
        // the resolver to consider them.
        let listed = repo.store().list_states().unwrap();
        assert!(
            listed.contains(&id_a),
            "state A must be indexed: {listed:?}"
        );
        assert!(listed.contains(&id_b), "state B must be indexed");

        let prefix = "hs-";

        let err = repo.resolve_state(prefix).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ambiguous state ID prefix"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains(prefix), "error should echo the prefix: {msg}");
        assert!(matches!(err, HeddleError::Conflict(_)));
    }
}
