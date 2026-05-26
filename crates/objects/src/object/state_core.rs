// SPDX-License-Identifier: Apache-2.0
//! Core state type.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Attribution, ChangeId, ContentHash, StateSignature, Status, Verification};

/// A state is an immutable snapshot with rich metadata.
///
/// On-disk encoding is rmp-serde's positional struct format (a fixed-length
/// tuple). This is sensitive to field order: inserting a field in the middle
/// of the tuple breaks every pre-existing on-disk state. The invariant we
/// keep going forward is:
///
/// > **New optional fields are added at the tail of the struct, below
/// > `status`, with `#[serde(default)]`.** Mid-struct inserts are
/// > forbidden. rmp-serde's positional deserializer tolerates missing
/// > trailing fields when they have a `Default` impl, so tail-only growth
/// > is forward-compatible automatically.
///
/// Required (non-optional) fields — `change_id`, `tree`, `parents`,
/// `attribution`, `created_at`, `status` — must never move. Optional fields
/// may be reordered only among themselves, and only at the tail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub change_id: ChangeId,
    #[serde(skip)]
    content_hash: Option<ContentHash>,
    pub tree: ContentHash,
    pub parents: Vec<ChangeId>,
    pub attribution: Attribution,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub created_at: DateTime<Utc>,
    pub verification: Option<Verification>,
    pub signature: Option<StateSignature>,
    pub status: Status,
    // --- tail-only optional fields below. Add new fields here, never above. ---
    #[serde(default)]
    pub provenance: Option<ContentHash>,
    #[serde(default)]
    pub logical_change_id: Option<ChangeId>,
    /// Optional context tree root for code annotations.
    #[serde(default)]
    pub context: Option<ContentHash>,
    /// Authoring timestamp for this state, when distinct from
    /// `created_at`.
    ///
    /// `created_at` is the *committer* time — when the state object
    /// came into being in its current form. We hash that into the
    /// state id so re-imports of the same git history produce
    /// deterministic Heddle hashes. But for blame display we usually
    /// want the *author* time — when someone actually wrote the
    /// change — which survives `git rebase`, cherry-pick, squash-
    /// merge, and `git commit --amend`. The `bridge git ingest`
    /// importer fills this from `git_commit.authored_at`; native
    /// heddle commits leave it `None` and blame falls back to
    /// `created_at`.
    #[serde(default)]
    pub authored_at: Option<DateTime<Utc>>,
    /// Content hash of the state's [`RiskSignalBlob`](crate::object::RiskSignalBlob),
    /// when present. Computed and persisted whenever risk signals fire on a
    /// state. `None` for states from before W1 and for states where no
    /// signals fired.
    ///
    /// Hash framing: a single `0` byte when `None`, `[1]` + 32-byte hash when
    /// `Some`. Legacy states without this field deserialize as `None` and
    /// hash byte-identical to before W1.
    #[serde(default)]
    pub risk_signals: Option<ContentHash>,
    /// Content hash of the state's [`ReviewSignaturesBlob`](crate::object::ReviewSignaturesBlob),
    /// when reviewers have signed off (read / agent-preview / agent-co-review).
    #[serde(default)]
    pub review_signatures: Option<ContentHash>,
    /// Content hash of the state's [`DiscussionsBlob`](crate::object::DiscussionsBlob),
    /// when discussions are anchored to this state.
    #[serde(default)]
    pub discussions: Option<ContentHash>,
    /// Content hash of the state's [`StructuredConflict`](crate::object::StructuredConflict),
    /// when this state captures an unresolved merge conflict as data.
    #[serde(default)]
    pub structured_conflicts: Option<ContentHash>,
}

impl State {
    pub fn new(tree: ContentHash, parents: Vec<ChangeId>, attribution: Attribution) -> Self {
        Self::new_snapshot(tree, parents, attribution)
    }

    pub fn new_snapshot(
        tree: ContentHash,
        parents: Vec<ChangeId>,
        attribution: Attribution,
    ) -> Self {
        let change_id = ChangeId::generate();
        Self::new_with_logical_change_id(tree, parents, attribution, change_id)
    }

    pub fn new_merge(tree: ContentHash, parents: Vec<ChangeId>, attribution: Attribution) -> Self {
        Self::new_snapshot(tree, parents, attribution)
    }

    pub fn new_refresh_of(
        tree: ContentHash,
        parents: Vec<ChangeId>,
        attribution: Attribution,
        logical_change_id: ChangeId,
    ) -> Self {
        Self::new_with_logical_change_id(tree, parents, attribution, logical_change_id)
    }

    pub fn new_fork_of(
        tree: ContentHash,
        parents: Vec<ChangeId>,
        attribution: Attribution,
    ) -> Self {
        Self::new_snapshot(tree, parents, attribution)
    }

    pub fn new_collapse_of(
        tree: ContentHash,
        parents: Vec<ChangeId>,
        attribution: Attribution,
    ) -> Self {
        Self::new_snapshot(tree, parents, attribution)
    }

    fn new_with_logical_change_id(
        tree: ContentHash,
        parents: Vec<ChangeId>,
        attribution: Attribution,
        logical_change_id: ChangeId,
    ) -> Self {
        Self {
            change_id: ChangeId::generate(),
            logical_change_id: Some(logical_change_id),
            content_hash: None,
            tree,
            parents,
            attribution,
            intent: None,
            confidence: None,
            created_at: Utc::now(),
            verification: None,
            signature: None,
            provenance: None,
            context: None,
            authored_at: None,
            risk_signals: None,
            review_signatures: None,
            discussions: None,
            structured_conflicts: None,
            status: Status::Draft,
        }
    }

    pub fn with_intent(mut self, intent: impl Into<String>) -> Self {
        self.intent = Some(intent.into());
        self.content_hash = None;
        self
    }

    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = Some(confidence.clamp(0.0, 1.0));
        self.content_hash = None;
        self
    }

    pub fn with_verification(mut self, verification: Verification) -> Self {
        self.verification = Some(verification);
        self.content_hash = None;
        self
    }

    pub fn with_signature(mut self, signature: StateSignature) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn with_provenance(mut self, provenance: ContentHash) -> Self {
        self.provenance = Some(provenance);
        self.content_hash = None;
        self
    }

    /// Set the context tree root.
    pub fn with_context(mut self, context: ContentHash) -> Self {
        self.context = Some(context);
        self.content_hash = None;
        self
    }

    /// Attach a [`RiskSignalBlob`](crate::object::RiskSignalBlob) hash.
    /// Render-time tick budgeting (selecting which signals to surface) is a
    /// view over this stored data, not part of storage itself.
    ///
    /// **Not part of the state hash.** Risk signals are derived data computed
    /// *about* a state from the diff against its parent; including them in
    /// identity would make the same logical state hash differently depending
    /// on which signals fired. That breaks every "is this the same state?"
    /// check in the system. See `authored_at` for the same pattern.
    pub fn with_risk_signals(mut self, risk_signals: ContentHash) -> Self {
        self.risk_signals = Some(risk_signals);
        self
    }

    /// Attach a [`ReviewSignaturesBlob`](crate::object::ReviewSignaturesBlob)
    /// hash. The state's authoring [`StateSignature`] is unaffected; review
    /// signatures live alongside it and accumulate over time.
    ///
    /// **Not part of the state hash.** Review signatures accumulate
    /// post-capture; including them in identity would mean every signature
    /// re-keys the state. See `authored_at` for the same pattern.
    pub fn with_review_signatures(mut self, review_signatures: ContentHash) -> Self {
        self.review_signatures = Some(review_signatures);
        self
    }

    /// Attach a [`DiscussionsBlob`](crate::object::DiscussionsBlob) hash.
    ///
    /// **Not part of the state hash.** Discussions evolve independently of
    /// the state they're anchored to — appending a turn must not change the
    /// state's identity. See `authored_at` for the same pattern.
    pub fn with_discussions(mut self, discussions: ContentHash) -> Self {
        self.discussions = Some(discussions);
        self
    }

    /// Attach a [`StructuredConflict`](crate::object::StructuredConflict) hash.
    ///
    /// **Not part of the state hash.** Conflict objects describe the merge's
    /// disagreement; the state's tree and parents already encode what's being
    /// merged. See `authored_at` for the same pattern.
    pub fn with_structured_conflicts(mut self, structured_conflicts: ContentHash) -> Self {
        self.structured_conflicts = Some(structured_conflicts);
        self
    }

    /// Record the authoring timestamp separately from `created_at`.
    /// Used by the git-ingest importer to preserve the distinction
    /// between "when the change was originally written" (authored)
    /// and "when this commit object came into being" (committer time,
    /// stored in `created_at` so re-imports stay deterministic).
    /// Native heddle commits leave this `None`; blame display then
    /// falls back to `created_at`.
    ///
    /// **Not part of the state hash.** `created_at` is what hashes;
    /// this field is purely metadata for display. A re-imported repo
    /// that picks up updated authored timestamps will produce the
    /// same Heddle State hashes as before.
    pub fn with_authored_at(mut self, timestamp: DateTime<Utc>) -> Self {
        self.authored_at = Some(timestamp);
        // Intentionally no `content_hash = None` here — authored_at is
        // not in the hash by design.
        self
    }

    pub fn with_status(mut self, status: Status) -> Self {
        self.status = status;
        self.content_hash = None;
        self
    }

    pub fn with_change_id(mut self, change_id: ChangeId) -> Self {
        let previous_change_id = self.change_id;
        self.change_id = change_id;
        if self.logical_change_id == Some(previous_change_id) || self.logical_change_id.is_none() {
            self.logical_change_id = Some(change_id);
            self.content_hash = None;
        }
        self
    }

    pub fn with_logical_change_id(mut self, logical_change_id: ChangeId) -> Self {
        self.logical_change_id = Some(logical_change_id);
        self.content_hash = None;
        self
    }

    pub fn logical_change_id(&self) -> ChangeId {
        self.logical_change_id.unwrap_or(self.change_id)
    }

    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.created_at = timestamp;
        self.content_hash = None;
        self
    }

    pub fn compute_hash(&self) -> ContentHash {
        let content_len = self.hash_len();
        ContentHash::compute_typed_with_len("state", content_len, |hasher| {
            self.update_hash(hasher);
        })
    }

    pub fn hash(&mut self) -> ContentHash {
        if self.content_hash.is_none() {
            self.content_hash = Some(self.compute_hash());
        }
        self.content_hash.expect("hash was just computed above")
    }

    pub fn is_root(&self) -> bool {
        self.parents.is_empty()
    }

    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }

    pub fn is_agent_authored(&self) -> bool {
        self.attribution.agent.is_some()
    }

    pub fn first_parent(&self) -> Option<&ChangeId> {
        self.parents.first()
    }

    fn hash_len(&self) -> u64 {
        let principal = &self.attribution.principal;
        let mut len = 0u64;

        len += 1;
        if self.logical_change_id.is_some() {
            len += 16;
        }

        len += self.tree.as_bytes().len() as u64;
        len += 4;
        len += (self.parents.len() * 16) as u64;

        len += principal.name.len() as u64 + 1;
        len += principal.email.len() as u64 + 1;

        len += 1;
        if let Some(agent) = &self.attribution.agent {
            len += agent.provider.len() as u64 + 1;
            len += agent.model.len() as u64 + 1;

            len += 1;
            if let Some(session_id) = &agent.session_id {
                len += session_id.len() as u64 + 1;
            }

            len += 1;
            if let Some(policy_id) = &agent.policy_id {
                len += policy_id.len() as u64 + 1;
            }
        }

        len += 1;
        if let Some(intent) = &self.intent {
            len += intent.len() as u64 + 1;
        }

        len += 1;
        if self.confidence.is_some() {
            len += 4;
        }

        len += 8;

        len += 1;
        if let Some(verification) = &self.verification {
            len += verification.hash_len() as u64;
        }

        len += 1;
        if self.provenance.is_some() {
            len += 32;
        }

        len += 1;
        if self.context.is_some() {
            len += 32;
        }

        len += 1;

        len
    }

    fn update_hash(&self, hasher: &mut blake3::Hasher) {
        let principal = &self.attribution.principal;

        if let Some(logical_change_id) = self.logical_change_id {
            hasher.update(&[1]);
            hasher.update(logical_change_id.as_bytes());
        } else {
            hasher.update(&[0]);
        }

        hasher.update(self.tree.as_bytes());
        hasher.update(&(self.parents.len() as u32).to_le_bytes());
        for parent in &self.parents {
            hasher.update(parent.as_bytes());
        }

        hasher.update(principal.name.as_bytes());
        hasher.update(&[0]);
        hasher.update(principal.email.as_bytes());
        hasher.update(&[0]);

        if let Some(agent) = &self.attribution.agent {
            hasher.update(&[1]);
            hasher.update(agent.provider.as_bytes());
            hasher.update(&[0]);
            hasher.update(agent.model.as_bytes());
            hasher.update(&[0]);
            write_optional_string(hasher, &agent.session_id);
            write_optional_string(hasher, &agent.segment_id);
            write_optional_string(hasher, &agent.policy_id);
        } else {
            hasher.update(&[0]);
        }

        write_optional_string(hasher, &self.intent);

        if let Some(confidence) = self.confidence {
            hasher.update(&[1]);
            hasher.update(&confidence.to_le_bytes());
        } else {
            hasher.update(&[0]);
        }

        hasher.update(&self.created_at.timestamp().to_le_bytes());

        if let Some(verification) = &self.verification {
            hasher.update(&[1]);
            verification.update_hasher(hasher);
        } else {
            hasher.update(&[0]);
        }

        if let Some(provenance) = self.provenance {
            hasher.update(&[1]);
            hasher.update(provenance.as_bytes());
        } else {
            hasher.update(&[0]);
        }

        if let Some(context) = self.context {
            hasher.update(&[1]);
            hasher.update(context.as_bytes());
        } else {
            hasher.update(&[0]);
        }

        hasher.update(&[self.status.to_byte()]);
    }
}

fn write_optional_string(hasher: &mut blake3::Hasher, value: &Option<String>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(value.as_bytes());
            hasher.update(&[0]);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Principal;

    fn sample_attribution() -> Attribution {
        Attribution::human(Principal::new("Alice", "alice@example.com"))
    }

    #[test]
    fn new_snapshot_sets_fresh_logical_identity() {
        let state =
            State::new_snapshot(ContentHash::compute(b"tree"), vec![], sample_attribution());
        let logical_change_id = state
            .logical_change_id
            .expect("snapshot should set logical identity");
        assert_ne!(state.logical_change_id(), state.change_id);
        assert_eq!(state.logical_change_id(), logical_change_id);
    }

    #[test]
    fn new_refresh_preserves_explicit_logical_identity() {
        let logical_change_id = ChangeId::from_bytes([7; 16]);
        let state = State::new_refresh_of(
            ContentHash::compute(b"tree"),
            vec![],
            sample_attribution(),
            logical_change_id,
        );
        assert_eq!(state.logical_change_id(), logical_change_id);
        assert_ne!(state.change_id, logical_change_id);
    }

    #[test]
    fn new_merge_uses_fresh_logical_identity() {
        let state = State::new_merge(
            ContentHash::compute(b"tree"),
            vec![ChangeId::from_bytes([1; 16]), ChangeId::from_bytes([2; 16])],
            sample_attribution(),
        );
        let logical_change_id = state
            .logical_change_id
            .expect("merge should set logical identity");
        assert_ne!(state.logical_change_id(), state.change_id);
        assert_eq!(state.logical_change_id(), logical_change_id);
        assert!(state.is_merge());
    }

    #[test]
    fn with_change_id_invalidates_cached_hash_when_logical_identity_changes() {
        let mut state =
            State::new_snapshot(ContentHash::compute(b"tree"), vec![], sample_attribution());
        let previous_change_id = state.change_id;
        state = state.with_logical_change_id(previous_change_id);
        let original_hash = state.hash();
        let replacement = ChangeId::from_bytes([9; 16]);

        let mut updated = state.with_change_id(replacement);

        assert_eq!(updated.logical_change_id(), replacement);
        assert_ne!(updated.hash(), original_hash);
        assert_eq!(updated.hash(), updated.compute_hash());
    }

    #[test]
    fn agent_segment_is_part_of_state_hash() {
        let principal = Principal::new("Alice", "alice@example.com");
        let attribution_a = Attribution::with_agent(
            principal.clone(),
            crate::object::Agent::new("openai", "gpt-5").with_session("sess-1", "seg-1"),
        );
        let attribution_b = Attribution::with_agent(
            principal,
            crate::object::Agent::new("openai", "gpt-5").with_session("sess-1", "seg-2"),
        );
        let tree = ContentHash::compute(b"tree");
        let timestamp = Utc::now();
        let logical_change_id = ChangeId::from_bytes([3; 16]);
        let state_a = State::new_snapshot(tree, vec![], attribution_a)
            .with_logical_change_id(logical_change_id)
            .with_timestamp(timestamp);
        let state_b = State::new_snapshot(tree, vec![], attribution_b)
            .with_logical_change_id(logical_change_id)
            .with_timestamp(timestamp);

        assert_ne!(state_a.compute_hash(), state_b.compute_hash());
    }

    fn sample_state() -> State {
        State::new_snapshot(ContentHash::compute(b"tree"), vec![], sample_attribution())
    }

    fn assert_mutator_invalidates_cached_hash(
        mut state: State,
        mutate: impl FnOnce(State) -> State,
    ) {
        let original_hash = state.hash();
        let mut updated = mutate(state);
        assert_ne!(updated.hash(), original_hash);
        assert_eq!(updated.hash(), updated.compute_hash());
    }

    #[test]
    fn with_intent_invalidates_cached_hash() {
        assert_mutator_invalidates_cached_hash(sample_state(), |state| {
            state.with_intent("capture intent")
        });
    }

    #[test]
    fn with_confidence_invalidates_cached_hash() {
        assert_mutator_invalidates_cached_hash(sample_state(), |state| state.with_confidence(0.9));
    }

    #[test]
    fn with_verification_invalidates_cached_hash() {
        assert_mutator_invalidates_cached_hash(sample_state(), |state| {
            state.with_verification(Verification::new().with_tests_passed(true))
        });
    }

    #[test]
    fn with_status_invalidates_cached_hash() {
        assert_mutator_invalidates_cached_hash(sample_state(), |state| {
            state.with_status(Status::Published)
        });
    }

    #[test]
    fn with_timestamp_invalidates_cached_hash() {
        assert_mutator_invalidates_cached_hash(sample_state(), |state| {
            state.with_timestamp(Utc::now() + chrono::Duration::seconds(1))
        });
    }

    /// Locks the contract that W1 tail-append fields (risk_signals,
    /// review_signatures, discussions, structured_conflicts) are NOT
    /// part of the state hash. Adding them to identity would mean the
    /// same logical state hashes differently depending on what signals
    /// fired, what review signatures arrived, or whether a discussion
    /// was anchored — which would break every "same state?" check in
    /// the system. Their persistence is independent of identity.
    #[test]
    fn w1_tail_fields_are_not_part_of_state_hash() {
        let mut bare = sample_state();
        let bare_hash = bare.hash();

        let mut decorated = sample_state()
            .with_change_id(bare.change_id)
            .with_logical_change_id(bare.logical_change_id())
            .with_risk_signals(ContentHash::compute(b"risk-signals-blob"))
            .with_review_signatures(ContentHash::compute(b"review-signatures-blob"))
            .with_discussions(ContentHash::compute(b"discussions-blob"))
            .with_structured_conflicts(ContentHash::compute(b"conflicts-blob"));
        decorated.created_at = bare.created_at;

        assert_eq!(
            decorated.hash(),
            bare_hash,
            "W1 tail fields must not affect the state hash"
        );
    }
}
