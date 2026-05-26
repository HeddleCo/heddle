// SPDX-License-Identifier: Apache-2.0
//! Action structure.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ActionId, Attribution, ChangeId, ContentHash, Operation, SemanticChange};

/// An action records an operation between states.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// Unique identifier (derived from content).
    #[serde(skip)]
    id: Option<ActionId>,

    /// Source state (None for initial state).
    pub from_state: Option<ChangeId>,

    /// Destination state.
    pub to_state: ChangeId,

    /// Type of operation.
    pub operation: Operation,

    /// Human-readable description.
    pub description: String,

    /// High-level semantic changes.
    pub semantic_changes: Vec<SemanticChange>,

    /// Who performed the action.
    pub attribution: Attribution,

    /// When the action was performed.
    pub timestamp: DateTime<Utc>,
}

impl Action {
    /// Create a new action.
    pub fn new(
        from_state: Option<ChangeId>,
        to_state: ChangeId,
        operation: Operation,
        description: impl Into<String>,
        attribution: Attribution,
    ) -> Self {
        Self {
            id: None,
            from_state,
            to_state,
            operation,
            description: description.into(),
            semantic_changes: Vec::new(),
            attribution,
            timestamp: Utc::now(),
        }
    }

    /// Add semantic changes.
    pub fn with_semantic_changes(mut self, changes: Vec<SemanticChange>) -> Self {
        self.semantic_changes = changes;
        self.id = None;
        self
    }

    /// Add a single semantic change.
    pub fn add_semantic_change(&mut self, change: SemanticChange) {
        self.semantic_changes.push(change);
        self.id = None;
    }

    /// Set the timestamp (for testing or importing).
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = timestamp;
        self.id = None;
        self
    }

    /// Compute the action ID from content.
    pub fn compute_id(&self) -> ActionId {
        #[derive(Serialize)]
        struct ActionIdentity<'a> {
            from_state: Option<&'a ChangeId>,
            to_state: &'a ChangeId,
            operation: &'a Operation,
            description: &'a str,
            semantic_changes: &'a [SemanticChange],
            attribution: &'a Attribution,
            timestamp_secs: i64,
            timestamp_nanos: u32,
        }

        let identity = ActionIdentity {
            from_state: self.from_state.as_ref(),
            to_state: &self.to_state,
            operation: &self.operation,
            description: &self.description,
            semantic_changes: &self.semantic_changes,
            attribution: &self.attribution,
            timestamp_secs: self.timestamp.timestamp(),
            timestamp_nanos: self.timestamp.timestamp_subsec_nanos(),
        };
        let data = serde_json::to_vec(&identity).expect("action identity should serialize");

        ActionId::from_hash(ContentHash::compute_typed("action", &data))
    }

    /// Get the action ID, computing it if necessary.
    pub fn id(&mut self) -> ActionId {
        if self.id.is_none() {
            self.id = Some(self.compute_id());
        }
        self.id.expect("id was just computed above")
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::object::{Agent, Principal};

    fn sample_action() -> Action {
        Action::new(
            None,
            ChangeId::from_bytes([1; 16]),
            Operation::Snapshot,
            "capture state",
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
    }

    #[test]
    fn compute_id_distinguishes_semantic_changes() {
        let base = sample_action().with_timestamp(Utc.timestamp_opt(1_700_000_000, 0).unwrap());
        let changed = base
            .clone()
            .with_semantic_changes(vec![SemanticChange::FileModified {
                path: "src/lib.rs".into(),
                classification: None,
                importance: None,
                confidence: None,
            }]);

        assert_ne!(base.compute_id(), changed.compute_id());
    }

    #[test]
    fn compute_id_distinguishes_attribution_and_subsecond_timestamps() {
        let base = sample_action().with_timestamp(Utc.timestamp_opt(1_700_000_000, 10).unwrap());
        let agent_authored = Action::new(
            None,
            ChangeId::from_bytes([1; 16]),
            Operation::Snapshot,
            "capture state",
            Attribution::with_agent(
                Principal::new("Alice", "alice@example.com"),
                Agent::new("openai", "gpt-5"),
            ),
        )
        .with_timestamp(Utc.timestamp_opt(1_700_000_000, 10).unwrap());
        let different_nanos =
            sample_action().with_timestamp(Utc.timestamp_opt(1_700_000_000, 11).unwrap());

        assert_ne!(base.compute_id(), agent_authored.compute_id());
        assert_ne!(base.compute_id(), different_nanos.compute_id());
    }

    #[test]
    fn mutators_invalidate_cached_action_id() {
        let mut action =
            sample_action().with_timestamp(Utc.timestamp_opt(1_700_000_000, 0).unwrap());
        let original_id = action.id();

        action.add_semantic_change(SemanticChange::DependencyAdded {
            name: "serde".to_string(),
            version: "1".to_string(),
        });

        assert_ne!(action.id(), original_id);

        let mut updated = action.with_timestamp(Utc.timestamp_opt(1_700_000_000, 42).unwrap());
        let updated_id = updated.id();

        assert_ne!(updated_id, original_id);
        assert_eq!(updated_id, updated.compute_id());
    }
}
