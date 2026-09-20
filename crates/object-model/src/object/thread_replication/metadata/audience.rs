//! Audience selection is independent of replication destinations and retention.
//! Matching an audience never substitutes for current capability authorization.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::super::invalid;
use crate::error::Result;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitee {
    pub principal_id: Uuid,
    /// None includes the principal's otherwise-authorized delegates. Some
    /// restricts this invitation to one exactly attributed agent.
    pub agent_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "invitees", rename_all = "snake_case")]
pub enum Audience {
    #[default]
    Owner,
    Invited(BTreeSet<Invitee>),
    /// Uses the Spool's current audience, never a cached membership snapshot.
    Spool,
}

impl Audience {
    pub fn validate(&self) -> Result<()> {
        if let Self::Invited(invitees) = self {
            if invitees.is_empty() || invitees.len() > 128 {
                return Err(invalid("Thread audience requires 1..128 invitees"));
            }
            for invitee in invitees {
                if invitee.principal_id.is_nil()
                    || invitee
                        .agent_id
                        .as_ref()
                        .is_some_and(|agent| agent.trim().is_empty() || agent.len() > 256)
                {
                    return Err(invalid("invalid Thread audience invitee"));
                }
            }
        }
        Ok(())
    }

    /// The owner account comes from verified original genesis admission, not
    /// the uploading principal, Spool owner, or current owner-key selector.
    pub fn includes(
        &self,
        owner: Uuid,
        principal: Uuid,
        agent_id: Option<&str>,
        in_spool_audience: bool,
    ) -> bool {
        if owner.is_nil() || principal.is_nil() {
            return false;
        }
        if principal == owner {
            return true;
        }
        self.includes_non_owner(principal, agent_id, in_spool_audience)
    }

    pub fn includes_non_owner(
        &self,
        principal: Uuid,
        agent_id: Option<&str>,
        in_spool_audience: bool,
    ) -> bool {
        if principal.is_nil() {
            return false;
        }
        match self {
            Self::Owner => false,
            Self::Spool => in_spool_audience,
            Self::Invited(invitees) => invitees.iter().any(|invitee| {
                invitee.principal_id == principal
                    && invitee
                        .agent_id
                        .as_deref()
                        .is_none_or(|id| Some(id) == agent_id)
            }),
        }
    }
}

/// Concurrent audience changes do not union grants. Access must satisfy every
/// unresolved candidate; an empty frontier has the owner-only default.
pub fn includes_frontier<'a>(
    candidates: impl IntoIterator<Item = &'a Audience>,
    owner: Uuid,
    principal: Uuid,
    agent_id: Option<&str>,
    in_spool_audience: bool,
) -> bool {
    let mut candidates = candidates.into_iter().peekable();
    if candidates.peek().is_none() {
        return Audience::Owner.includes(owner, principal, agent_id, in_spool_audience);
    }
    candidates.all(|policy| policy.includes(owner, principal, agent_id, in_spool_audience))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_membership_never_opens_owner_or_invited_audiences() {
        let owner = Uuid::from_u128(1);
        let guest = Uuid::from_u128(2);
        let other = Uuid::from_u128(3);
        let invited = Audience::Invited(BTreeSet::from([Invitee {
            principal_id: guest,
            agent_id: Some("reviewer".into()),
        }]));
        assert!(!Audience::Owner.includes(owner, guest, None, true));
        assert!(!invited.includes(owner, other, Some("reviewer"), true));
        assert!(!invited.includes(owner, guest, Some("different-agent"), true));
        assert!(!invited.includes(owner, guest, None, true));
        assert!(invited.includes(owner, guest, Some("reviewer"), false));
        assert!(Audience::Owner.includes(owner, owner, Some("delegated-agent"), false));
        assert!(!Audience::Spool.includes(owner, guest, None, false));
        assert!(Audience::Spool.includes(owner, guest, None, true));
    }

    #[test]
    fn concurrent_audiences_intersect_and_missing_policy_is_owner_only() {
        let owner = Uuid::from_u128(1);
        let guest = Uuid::from_u128(2);
        assert!(!includes_frontier([], owner, guest, None, true));
        assert!(includes_frontier([], owner, owner, None, false));
        assert!(!includes_frontier(
            [&Audience::Spool, &Audience::Owner],
            owner,
            guest,
            None,
            true,
        ));
        assert!(includes_frontier(
            [&Audience::Spool, &Audience::Owner],
            owner,
            owner,
            None,
            false,
        ));
        assert!(!includes_frontier([], Uuid::nil(), Uuid::nil(), None, true));
    }
}
