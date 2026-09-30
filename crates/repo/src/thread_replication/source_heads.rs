// SPDX-License-Identifier: Apache-2.0
//! Concurrent source heads on one Thread.
//!
//! Two writers publishing divergent captures on one Thread is a supported
//! state: every non-dominated head stays in the local replica until a later
//! capture names it as a parent. Clone and pull must still put one head in the
//! worktree, so they use one documented default:
//!
//! 1. The local Thread tip when it is itself a head (pull never moves your own
//!    work off the worktree to adopt someone else's head).
//! 2. Otherwise the heads that fast-forward the local tip; among several, the
//!    greatest State ID.
//! 3. With no local tip (a fresh clone), the greatest State ID.
//!
//! The State ID is content-intrinsic, so every replica picks the same head
//! regardless of wire order, arrival time or clock. Timestamps never select a
//! winner. Choosing a default discards nothing: the other heads remain in the
//! replica, and [`Repository::native_source_heads`] reports them until a pick
//! or merge resolves them.
use std::collections::BTreeSet;

use objects::{
    object::{Attribution, State, StateId, StateVisibility, ThreadName, VisibilityTier},
    store::ObjectStore,
};

use super::{Error, Result};
use crate::{CommitGraphIndex, Repository};

/// Why a clone or pull materialized its head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultHeadRule {
    /// The Thread has one head.
    Sole,
    /// The local Thread tip is one of the heads.
    LocalTip,
    /// The greatest State ID among heads that fast-forward the local tip.
    LocalLineage,
    /// The greatest State ID among all heads.
    GreatestStateId,
}

impl DefaultHeadRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sole => "sole_head",
            Self::LocalTip => "local_tip",
            Self::LocalLineage => "local_lineage_greatest_state_id",
            Self::GreatestStateId => "greatest_state_id",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Sole,
            Self::LocalTip,
            Self::LocalLineage,
            Self::GreatestStateId,
        ]
        .into_iter()
        .find(|rule| rule.as_str() == value)
    }

    /// One sentence for human output.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Sole => "the Thread's only head",
            Self::LocalTip => "your local tip is one of the heads, so it stays checked out",
            Self::LocalLineage => {
                "the greatest State ID among heads that fast-forward your local tip"
            }
            Self::GreatestStateId => "the greatest State ID; arrival order and clocks never pick",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultSourceHead {
    pub head: StateId,
    pub rule: DefaultHeadRule,
}

/// The default among `heads` when no local repository is at hand (a hosted
/// ref advertisement). Matches rule 3 of [`default_source_head`].
pub fn greatest_source_head(heads: &BTreeSet<StateId>) -> Option<StateId> {
    heads.last().copied()
}

/// Apply the module's default-head rule. `local_tip` is the local Thread ref
/// before the transfer, when one exists.
pub fn default_source_head(
    repo: &Repository,
    heads: &BTreeSet<StateId>,
    local_tip: Option<StateId>,
) -> Result<Option<DefaultSourceHead>> {
    let Some(greatest) = greatest_source_head(heads) else {
        return Ok(None);
    };
    if heads.len() == 1 {
        return Ok(Some(DefaultSourceHead {
            head: greatest,
            rule: DefaultHeadRule::Sole,
        }));
    }
    let Some(tip) = local_tip else {
        return Ok(Some(DefaultSourceHead {
            head: greatest,
            rule: DefaultHeadRule::GreatestStateId,
        }));
    };
    if heads.contains(&tip) {
        return Ok(Some(DefaultSourceHead {
            head: tip,
            rule: DefaultHeadRule::LocalTip,
        }));
    }
    let mut graph = CommitGraphIndex::new(repo);
    let mut lineage = None;
    for head in heads.iter().rev() {
        if graph
            .is_ancestor(&tip, head)
            .map_err(|error| Error::Invalid(error.to_string()))?
        {
            lineage = Some(*head);
            break;
        }
    }
    Ok(Some(match lineage {
        Some(head) => DefaultSourceHead {
            head,
            rule: DefaultHeadRule::LocalLineage,
        },
        None => DefaultSourceHead {
            head: greatest,
            rule: DefaultHeadRule::GreatestStateId,
        },
    }))
}

impl Repository {
    /// Current source heads of a named local Thread, from the indexed
    /// frontier. `None` when the Thread has no native identity (a Git
    /// overlay checkout, or a Thread never captured natively).
    pub fn native_source_heads(&self, name: &str) -> Result<Option<BTreeSet<StateId>>> {
        let replica = match self.native_thread(name) {
            Ok(replica) => replica,
            Err(error) if super::local::is_missing_native_identity(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(replica.source_head_revisions()?))
    }

    /// Resolve every concurrent source head of the attached Thread `name` to
    /// `selected`. The result is one capture whose tree is exactly the
    /// selected head's tree and whose parents name every head, the selected
    /// head first, so no alternative is discarded: each stays in ancestry.
    ///
    /// The caller has checked that HEAD is attached to `name` and that the
    /// worktree matches the Thread tip.
    pub fn pick_native_source_head(
        &self,
        name: &str,
        selected: StateId,
        attribution: Attribution,
        intent: String,
    ) -> Result<State> {
        let heads = self
            .native_source_heads(name)?
            .ok_or_else(|| Error::Invalid(format!("Thread {name:?} has no native identity")))?;
        if heads.len() < 2 {
            return Err(Error::Invalid(format!(
                "Thread {name:?} has no alternative source heads"
            )));
        }
        if !heads.contains(&selected) {
            return Err(Error::Invalid(format!(
                "{} is not a source head of Thread {name:?}",
                selected.to_string_full()
            )));
        }
        let thread = ThreadName::new(name);
        let tip = self
            .refs()
            .get_thread(&thread)?
            .ok_or_else(|| Error::Invalid(format!("Thread {name:?} has no local tip")))?;
        let source = self
            .store()
            .get_state(&selected)?
            .ok_or_else(|| Error::Invalid("selected source head is not available".into()))?;
        let mut parents = vec![selected];
        parents.extend(heads.iter().copied().filter(|head| *head != selected));
        let state = State::new_merge(source.tree, parents, attribution).with_intent(intent);
        self.put_authored_state(&state)?;
        let mut tier = self.resolve_capture_default_visibility();
        for parent in &state.parents {
            tier = objects::object::thread_replication::local_integration::intersect_visibility(
                &tier,
                &self
                    .effective_visibility_tier(parent)
                    .map_err(|error| Error::Invalid(error.to_string()))?,
            )?;
        }
        if tier != VisibilityTier::Public {
            self.put_state_visibility_if_absent(StateVisibility {
                state: state.id(),
                tier,
                embargo_until: None,
                declarer: state.attribution.principal.clone(),
                declared_at: state.created_at,
                signature: None,
                supersedes: None,
            })
            .map_err(|error| Error::Invalid(error.to_string()))?;
        }
        // Admit before the ref moves: a ref ahead of native admission wedges
        // later captures.
        self.record_native_source(name, state.id())?;
        self.restore_worktree_state_only(&state.id(), Some(&tip))?;
        self.commit_snapshot_atomic_with_capture_visibility(
            &state.id(),
            Some(tip),
            Some(&thread),
            false,
        )?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{Principal, Tree};

    use super::*;

    fn state(repo: &Repository, parents: Vec<StateId>, intent: &str) -> StateId {
        let tree = repo.store().put_tree(&Tree::new()).expect("empty tree");
        let state = State::new_snapshot(
            tree,
            parents,
            Attribution::human(Principal::new("Writer", "writer@example.test")),
        )
        .with_intent(intent);
        repo.store().put_state(&state).expect("state");
        state.id()
    }

    fn rule(
        repo: &Repository,
        heads: &BTreeSet<StateId>,
        local_tip: Option<StateId>,
    ) -> Option<DefaultSourceHead> {
        default_source_head(repo, heads, local_tip).expect("default head")
    }

    #[test]
    fn default_head_prefers_local_tip_then_its_lineage_then_greatest_state_id() {
        let dir = tempfile::TempDir::new().expect("repo dir");
        let repo = crate::init_test_repository(dir.path()).expect("repo");
        let base = state(&repo, Vec::new(), "base");
        let left = state(&repo, vec![base], "left");
        let right = state(&repo, vec![base], "right");
        let unrelated = state(&repo, Vec::new(), "unrelated");
        let heads = BTreeSet::from([left, right]);
        let greatest = left.max(right);

        assert_eq!(rule(&repo, &BTreeSet::new(), None), None);
        assert_eq!(
            rule(&repo, &BTreeSet::from([left]), Some(unrelated)),
            Some(DefaultSourceHead {
                head: left,
                rule: DefaultHeadRule::Sole,
            })
        );
        for tip in [None, Some(unrelated)] {
            assert_eq!(
                rule(&repo, &heads, tip),
                Some(DefaultSourceHead {
                    head: greatest,
                    rule: DefaultHeadRule::GreatestStateId,
                }),
                "no local lineage falls back to the content-intrinsic order"
            );
        }
        for own in [left, right] {
            assert_eq!(
                rule(&repo, &heads, Some(own)),
                Some(DefaultSourceHead {
                    head: own,
                    rule: DefaultHeadRule::LocalTip,
                }),
                "a pull never moves the caller off its own head"
            );
        }
        assert_eq!(
            rule(&repo, &heads, Some(base)),
            Some(DefaultSourceHead {
                head: greatest,
                rule: DefaultHeadRule::LocalLineage,
            })
        );
        let ahead = state(&repo, vec![left], "ahead of left");
        let lineage = BTreeSet::from([ahead, right]);
        assert_eq!(
            rule(&repo, &lineage, Some(left)),
            Some(DefaultSourceHead {
                head: ahead,
                rule: DefaultHeadRule::LocalLineage,
            }),
            "the head that fast-forwards the tip wins over a greater State ID"
        );
        for value in [
            DefaultHeadRule::Sole,
            DefaultHeadRule::LocalTip,
            DefaultHeadRule::LocalLineage,
            DefaultHeadRule::GreatestStateId,
        ] {
            assert_eq!(DefaultHeadRule::parse(value.as_str()), Some(value));
        }
    }
}
