// SPDX-License-Identifier: Apache-2.0
//! Core state type and its leaf value types (Status, StateSignature,
//! SignatureStatus, Verification).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Attribution, ChangeId, ContentHash, Principal};

// ── Status ──────────────────────────────────────────────────────────

/// Lifecycle status of a state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Draft,
    Published,
}

impl Status {
    pub fn to_byte(&self) -> u8 {
        match self {
            Status::Draft => 0,
            Status::Published => 1,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Status::Draft),
            1 => Some(Status::Published),
            _ => None,
        }
    }
}

// ── StateSignature ──────────────────────────────────────────────────

/// Signature information for a state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSignature {
    pub algorithm: String,
    pub public_key: String,
    pub signature: String,
}

impl StateSignature {
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }
}

/// Signature verification result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureStatus {
    Valid,
    Invalid,
    Unsigned,
}

impl SignatureStatus {
    pub fn is_valid(self) -> bool {
        self == SignatureStatus::Valid
    }

    pub fn is_unsigned(self) -> bool {
        self == SignatureStatus::Unsigned
    }
}

// ── Verification ────────────────────────────────────────────────────

/// Verification information for a state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    pub tests_passed: Option<bool>,
    pub tests_failed: Option<u32>,
    pub coverage_pct: Option<f32>,
    pub coverage_delta: Option<f32>,
    pub lint_warnings: Option<u32>,
    #[serde(default)]
    pub custom: BTreeMap<String, serde_json::Value>,
}

impl Verification {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tests_passed(mut self, passed: bool) -> Self {
        self.tests_passed = Some(passed);
        self
    }

    pub fn with_tests_failed(mut self, failed: u32) -> Self {
        self.tests_failed = Some(failed);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.tests_passed.is_none()
            && self.tests_failed.is_none()
            && self.coverage_pct.is_none()
            && self.coverage_delta.is_none()
            && self.lint_warnings.is_none()
            && self.custom.is_empty()
    }

    pub(crate) fn hash_len(&self) -> usize {
        let mut len = 0;
        len += 1 + self.tests_passed.map(|_| 1).unwrap_or(0);
        len += 1 + self.tests_failed.map(|_| 4).unwrap_or(0);
        len += 1 + self.coverage_pct.map(|_| 4).unwrap_or(0);
        len += 1 + self.coverage_delta.map(|_| 4).unwrap_or(0);
        len += 1 + self.lint_warnings.map(|_| 4).unwrap_or(0);
        len += 4;
        for (key, value) in &self.custom {
            let value_bytes = serde_json::to_vec(value).unwrap_or_default();
            len += 4 + key.len();
            len += 4 + value_bytes.len();
        }
        len
    }

    pub(crate) fn update_hasher(&self, hasher: &mut blake3::Hasher) {
        let tests_passed = self.tests_passed.map(u8::from);
        write_optional_u8(hasher, tests_passed);
        write_optional_u32(hasher, self.tests_failed);
        write_optional_f32(hasher, self.coverage_pct);
        write_optional_f32(hasher, self.coverage_delta);
        write_optional_u32(hasher, self.lint_warnings);
        let custom_len = self.custom.len() as u32;
        hasher.update(&custom_len.to_le_bytes());
        for (key, value) in &self.custom {
            let key_bytes = key.as_bytes();
            let value_bytes = serde_json::to_vec(value).unwrap_or_default();
            hasher.update(&(key_bytes.len() as u32).to_le_bytes());
            hasher.update(key_bytes);
            hasher.update(&(value_bytes.len() as u32).to_le_bytes());
            hasher.update(&value_bytes);
        }
    }
}

fn write_optional_u8(hasher: &mut blake3::Hasher, value: Option<u8>) {
    match value {
        Some(v) => {
            hasher.update(&[1]);
            hasher.update(&[v]);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn write_optional_u32(hasher: &mut blake3::Hasher, value: Option<u32>) {
    match value {
        Some(v) => {
            hasher.update(&[1]);
            hasher.update(&v.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn write_optional_f32(hasher: &mut blake3::Hasher, value: Option<f32>) {
    match value {
        Some(v) => {
            hasher.update(&[1]);
            hasher.update(&v.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

// ── State ───────────────────────────────────────────────────────────

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
    /// came into being in its current form. `authored_at` is the
    /// *author* time — when someone actually wrote the change — which
    /// survives `git rebase`, cherry-pick, squash-merge, and `git
    /// commit --amend`. The `bridge git ingest`/`import` importers fill
    /// this from the git author time; native heddle commits leave it
    /// `None` and blame falls back to `created_at`.
    ///
    /// **Part of the state hash (#564 de-lossy step 1).** Author time
    /// is part of a git commit's identity: two commits that differ
    /// *only* by author timestamp are distinct git objects, so folding
    /// it into the hash keeps them from dedup-colliding to one State in
    /// the content-addressed store. `None` hashes as a single absence
    /// byte, so native commits are unaffected beyond the format bump.
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
    // --- git-fidelity fields (#564 de-lossy step 1, #565) ---
    //
    // These preserve the parts of an imported git commit that Heddle's
    // model used to drop, so a commit can be byte-reconstructed later
    // (#566/#567) and the git mirror can be eliminated (#568). UNLIKE the
    // W1 tail fields above, these ARE part of the content hash (see
    // `update_hash`): two git-distinct commits that differ only in
    // committer, timezone, verbatim message, gpgsig, or extra headers must
    // hash differently so they can't dedup-collide in the content-addressed
    // store. They are still tail-append + `#[serde(default)]` so legacy
    // on-disk states keep deserializing.
    /// The git committer identity, when distinct from the author
    /// ([`Attribution::principal`]). Git records both an author (who wrote
    /// the change) and a committer (who created this commit object); for
    /// rebased / cherry-picked / amended commits the two differ. `None`
    /// for native heddle commits and for legacy imports from before #565.
    #[serde(default)]
    pub committer: Option<Principal>,
    /// Timezone offset (seconds east of UTC) of the *author* timestamp
    /// ([`State::authored_at`] / `created_at` fallback). Git stores the
    /// author's local offset (e.g. `+0000`, `-0700`); Heddle used to
    /// discard it. `0` for native commits and legacy imports.
    #[serde(default)]
    pub authored_tz_offset: i32,
    /// Timezone offset (seconds east of UTC) of the *committer* timestamp
    /// (`created_at`). `0` for native commits and legacy imports.
    #[serde(default)]
    pub committer_tz_offset: i32,
    /// The verbatim git commit message body (everything after the header
    /// block), preserved exactly so reconstruction is byte-stable. Distinct
    /// from `intent`, which is the trimmed first line surfaced in the UI.
    /// `None` for native commits and legacy imports.
    ///
    /// Stored as raw bytes, NOT a `String`: a commit with a non-UTF8
    /// `encoding` (latin-1, shift-jis, …) carries message bytes that are not
    /// valid UTF-8 (e.g. `0xe9` for latin-1 `é`); a `String` could not
    /// round-trip them byte-identically. (non-UTF8 author/committer identity
    /// *names* are not yet byte-preserved — `Principal` is still `String`; see
    /// #564.)
    #[serde(default)]
    pub raw_message: Option<Vec<u8>>,
    /// Every git commit header beyond the ones Heddle models natively
    /// (tree/parents/author/committer), in their original order. ORDER IS
    /// LOAD-BEARING for #566 byte-exactness — this is a `Vec`, never a map.
    /// Empty for native commits and legacy imports.
    ///
    /// `gpgsig` is just one of these headers and is kept INLINE at its
    /// captured ordinal (not split into a separate field): when a commit's
    /// extension headers are in non-canonical order — e.g. `x-custom`, then
    /// `gpgsig`, then `mergetag` — splitting gpgsig out would lose its
    /// position and break byte-identical reconstruction. The serialization
    /// source of truth for the signature is its position here (spike §3).
    ///
    /// Both the header name and value are raw bytes (`Vec<u8>`), NOT
    /// `String`s: extra-header VALUES (a `mergetag` payload is a full tag
    /// object; custom headers; gpgsig armor) can be non-UTF8, so a
    /// `String` would force a lossy `to_string()` that destroys those bytes.
    /// Names are ASCII by git's spec but are bytes too so the whole tuple is
    /// byte-exact and no conversion sneaks in.
    #[serde(default)]
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
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
            committer: None,
            authored_tz_offset: 0,
            committer_tz_offset: 0,
            raw_message: None,
            extra_headers: Vec::new(),
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
    /// **Part of the state hash (#564 de-lossy step 1)** — see the
    /// `authored_at` field docs and `update_hash`.
    pub fn with_authored_at(mut self, timestamp: DateTime<Utc>) -> Self {
        self.authored_at = Some(timestamp);
        self.content_hash = None;
        self
    }

    /// Record the git committer identity (distinct from the author).
    ///
    /// **Part of the state hash** — see the `committer` field docs and
    /// `update_hash`. #564 de-lossy step 1.
    pub fn with_committer(mut self, committer: Principal) -> Self {
        self.committer = Some(committer);
        self.content_hash = None;
        self
    }

    /// Record the author/committer timezone offsets (seconds east of UTC).
    /// **Part of the state hash.** #564 de-lossy step 1.
    pub fn with_tz_offsets(mut self, authored: i32, committer: i32) -> Self {
        self.authored_tz_offset = authored;
        self.committer_tz_offset = committer;
        self.content_hash = None;
        self
    }

    /// Record the verbatim git commit message body, as raw bytes (so a
    /// non-UTF8 message round-trips byte-identically; see the `raw_message`
    /// field docs). **Part of the state hash.** #564 de-lossy step 1.
    pub fn with_raw_message(mut self, raw_message: impl AsRef<[u8]>) -> Self {
        self.raw_message = Some(raw_message.as_ref().to_vec());
        self.content_hash = None;
        self
    }

    /// Record the ordered remaining git commit headers as raw bytes. ORDER
    /// IS LOAD-BEARING (#566). **Part of the state hash.** #564 de-lossy
    /// step 1.
    pub fn with_extra_headers(mut self, extra_headers: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        self.extra_headers = extra_headers;
        self.content_hash = None;
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

    /// The pre-#565 content hash: the hash a state had BEFORE the git-fidelity
    /// fields were folded into identity (the format bump in #565). It omits the
    /// trailing fidelity block from both the hashed bytes AND the content-length
    /// prefix, exactly as the old code did — so for a state signed before the
    /// bump, this reproduces the hash its `StateSignature` was actually made
    /// over.
    ///
    /// The #570 fidelity backfill verifies an existing signature against this
    /// (in addition to the current `compute_hash`) before re-signing: a legacy
    /// signature was made over THIS hash, not the post-bump one, so checking
    /// only the new hash would wrongly reject a valid legacy signature as
    /// unreproducible. #565 only *appended* the fidelity block to `hash_len` /
    /// `update_hash`, so stopping before it is a faithful pre-bump hash.
    pub fn compute_hash_pre_fidelity(&self) -> ContentHash {
        let content_len = self.hash_len_core();
        ContentHash::compute_typed_with_len("state", content_len, |hasher| {
            self.update_hash_core(hasher);
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
        self.hash_len_core() + self.hash_len_fidelity()
    }

    /// Hashed length of the pre-#565 fields (everything through the status
    /// byte). Mirrors [`Self::update_hash_core`]. Split out so the pre-bump
    /// hash ([`Self::compute_hash_pre_fidelity`]) can be reproduced exactly.
    fn hash_len_core(&self) -> u64 {
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

    /// Hashed length of the appended git-fidelity block (#565). Mirrors
    /// [`Self::update_hash_fidelity`] byte-for-byte. Kept separate from
    /// [`Self::hash_len_core`] so the pre-bump hash can omit it exactly.
    fn hash_len_fidelity(&self) -> u64 {
        let mut len = 0u64;

        // git-fidelity fields (#564 step 1). Must mirror `update_hash`
        // byte-for-byte. committer: 1 tag byte + (name+NUL, email+NUL).
        len += 1;
        if let Some(committer) = &self.committer {
            len += committer.name.len() as u64 + 1;
            len += committer.email.len() as u64 + 1;
        }
        // both tz offsets: i32 LE, always present.
        len += 4;
        len += 4;
        // authored_at (author time): 1 tag byte + (i64 LE when Some).
        len += 1;
        if self.authored_at.is_some() {
            len += 8;
        }
        // raw_message: optional-bytes framing (1 tag + u32 len + bytes) — a
        // length prefix, not NUL-termination, since the message can contain
        // NUL bytes (it's byte-typed for non-UTF8 fidelity).
        len += 1;
        if let Some(raw_message) = &self.raw_message {
            len += 4 + raw_message.len() as u64;
        }
        // extra_headers (gpgsig rides inline here at its captured position):
        // u32 count, then per pair u32 key_len+key, u32 val_len+val.
        len += 4;
        for (key, value) in &self.extra_headers {
            len += 4 + key.len() as u64;
            len += 4 + value.len() as u64;
        }

        len
    }

    fn update_hash(&self, hasher: &mut blake3::Hasher) {
        self.update_hash_core(hasher);
        self.update_hash_fidelity(hasher);
    }

    /// Hash the pre-#565 fields (everything through the status byte). Mirrors
    /// [`Self::hash_len_core`]. The pre-bump hash
    /// ([`Self::compute_hash_pre_fidelity`]) is exactly this with no fidelity
    /// block appended.
    fn update_hash_core(&self, hasher: &mut blake3::Hasher) {
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

    /// Hash the appended git-fidelity block (#565). Mirrors
    /// [`Self::hash_len_fidelity`]. Kept separate from
    /// [`Self::update_hash_core`] so a pre-bump hash can omit it exactly.
    ///
    /// git-fidelity fields (#564 de-lossy step 1, #565) are DELIBERATELY part
    /// of the content hash — the opposite of the W1 tail fields. Two git
    /// commits that differ only in committer, author/committer time, timezone,
    /// verbatim message, or extra headers (gpgsig included) are distinct git
    /// objects; folding these into identity prevents them from dedup-colliding
    /// to one State in the content-addressed store. This re-hashes every
    /// pre-#565 state (a real format bump; acceptable pre-0.3). Keep this in
    /// sync with `hash_len_fidelity`.
    fn update_hash_fidelity(&self, hasher: &mut blake3::Hasher) {
        if let Some(committer) = &self.committer {
            hasher.update(&[1]);
            hasher.update(committer.name.as_bytes());
            hasher.update(&[0]);
            hasher.update(committer.email.as_bytes());
            hasher.update(&[0]);
        } else {
            hasher.update(&[0]);
        }

        hasher.update(&self.authored_tz_offset.to_le_bytes());
        hasher.update(&self.committer_tz_offset.to_le_bytes());

        // Author time (#564): committer time is hashed above as created_at;
        // author time is the other half of a git commit's temporal identity.
        if let Some(authored_at) = self.authored_at {
            hasher.update(&[1]);
            hasher.update(&authored_at.timestamp().to_le_bytes());
        } else {
            hasher.update(&[0]);
        }

        write_optional_bytes(hasher, &self.raw_message);

        // extra_headers (gpgsig is one of these, kept inline at its position).
        hasher.update(&(self.extra_headers.len() as u32).to_le_bytes());
        for (key, value) in &self.extra_headers {
            hasher.update(&(key.len() as u32).to_le_bytes());
            hasher.update(key);
            hasher.update(&(value.len() as u32).to_le_bytes());
            hasher.update(value);
        }
    }
}

/// Length-prefixed optional-bytes framing for the hash: `[1] + u32-LE len +
/// bytes` when `Some`, a single `[0]` when `None`. Unlike
/// [`write_optional_string`]'s NUL-terminated framing this is binary-safe —
/// `raw_message` can contain NUL bytes, so a length prefix (not a terminator)
/// is required to keep the hash unambiguous.
fn write_optional_bytes(hasher: &mut blake3::Hasher, value: &Option<Vec<u8>>) {
    match value {
        Some(bytes) => {
            hasher.update(&[1]);
            hasher.update(&(bytes.len() as u32).to_le_bytes());
            hasher.update(bytes);
        }
        None => {
            hasher.update(&[0]);
        }
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

/// Parse the *extension* headers from a raw git commit object's content bytes
/// (the bytes `git cat-file commit <sha>` prints — i.e. gix's `Commit::data`),
/// in their exact on-the-wire order, ready to store in [`State::extra_headers`].
///
/// A commit's header block runs from the start of the content up to the first
/// blank line (the header/body separator). Its leading headers are always, in
/// fixed order, `tree`, zero-or-more `parent`, `author`, `committer`; Heddle
/// models those natively. Every header **after** `committer` is an extension
/// header (`encoding`, `gpgsig`, `mergetag`, or any unknown/future name) and is
/// returned here as a `(name, value)` byte pair at its real position.
///
/// **This is the single source of truth for extension-header order and bytes.**
/// Both git import paths (the CLI bridge and the ingest walker) build
/// `extra_headers` from it. The alternative — stitching the vec back together
/// from a decoder's *typed* accessors (gix surfaces `encoding`, and historically
/// `gpgsig`, as fields *outside* its `extra_headers`) — silently reorders the
/// headers git happens to model as typed fields, which breaks #566 byte-exact
/// reconstruction. So we never consult those typed accessors for position; the
/// raw header block is authoritative. (#564 de-lossy step 1 — close-the-class.)
///
/// Folded continuation lines (a value line beginning with a single space
/// `0x20`, used by `gpgsig`/`mergetag`) are **unfolded**: each continuation
/// contributes a `\n` plus the line with exactly one leading space stripped, so
/// the stored value holds the value's real internal newlines with no trailing
/// newline. The serializer (#566) re-folds by mapping every `\n` back to `\n `
/// (spike §2). A "blank" line inside an armored value is ` \n` on the wire (one
/// space), so it unfolds to an empty segment — never confused with the
/// header/body separator, which is a truly empty line.
pub fn parse_commit_extension_headers(commit_content: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    // The header block ends at the first *empty* line. Folded "blank" lines
    // inside an armored value are ` \n` (a single space), never empty, so the
    // first `\n\n` reliably marks the header/body boundary.
    let header_block = match find_subslice(commit_content, b"\n\n") {
        Some(idx) => &commit_content[..idx],
        // No separator (malformed / header-only) — treat all of it as headers.
        None => commit_content,
    };

    // Collect every logical header (name, unfolded value) in order; the
    // extension headers are the ones after the `committer` line.
    let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for line in header_block.split(|&b| b == b'\n') {
        if line.first() == Some(&b' ') {
            // Continuation of the current header value: restore the newline
            // that folding replaced and strip exactly one leading space.
            if let Some((_, value)) = headers.last_mut() {
                value.push(b'\n');
                value.extend_from_slice(&line[1..]);
            }
            // A continuation with no preceding header is malformed git; skip it
            // rather than panic.
            continue;
        }
        // New header: `name<SP>value`. A header line with no space is degenerate
        // (git never emits one in this region) — record it with an empty value
        // so no bytes are silently dropped.
        let (name, value) = match line.iter().position(|&b| b == b' ') {
            Some(sp) => (line[..sp].to_vec(), line[sp + 1..].to_vec()),
            None => (line.to_vec(), Vec::new()),
        };
        headers.push((name, value));
    }

    // Extension headers are everything strictly after `committer`. git always
    // emits exactly one committer line ahead of the extension headers; if it is
    // somehow absent, fall back to excluding the four core names so nothing is
    // silently dropped or mis-captured.
    match headers.iter().position(|(name, _)| name == b"committer") {
        Some(idx) => headers.split_off(idx + 1),
        None => headers
            .into_iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_slice(),
                    b"tree" | b"parent" | b"author" | b"committer"
                )
            })
            .collect(),
    }
}

/// Index of the first occurrence of `needle` in `haystack`, or `None`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
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

    /// The inverse of `w1_tail_fields_are_not_part_of_state_hash`: the
    /// git-fidelity fields (#564 step 1) MUST be part of the hash so two
    /// git-distinct commits can't dedup-collide. Each field, set in
    /// isolation, must move the hash.
    #[test]
    fn fidelity_fields_are_part_of_state_hash() {
        let base = sample_state();
        let base_hash = base.compute_hash();

        let with_committer = sample_state()
            .with_change_id(base.change_id)
            .with_logical_change_id(base.logical_change_id());
        let mut with_committer = with_committer
            .with_committer(Principal::new("Carol", "carol@example.com"));
        with_committer.created_at = base.created_at;
        assert_ne!(
            with_committer.hash(),
            base_hash,
            "committer must affect the state hash"
        );

        for mutate in [
            |s: State| s.with_tz_offsets(3600, -7200),
            |s: State| s.with_authored_at(Utc::now() + chrono::Duration::seconds(1)),
            |s: State| s.with_raw_message("verbatim body\n"),
            // gpgsig now rides inline in extra_headers at its captured position.
            |s: State| {
                s.with_extra_headers(vec![(
                    b"gpgsig".to_vec(),
                    b"-----BEGIN PGP SIGNATURE-----\n".to_vec(),
                )])
            },
            |s: State| s.with_extra_headers(vec![(b"mergetag".to_vec(), b"x".to_vec())]),
        ] {
            let seeded = sample_state()
                .with_change_id(base.change_id)
                .with_logical_change_id(base.logical_change_id());
            let mut decorated = mutate(seeded);
            decorated.created_at = base.created_at;
            assert_ne!(
                decorated.hash(),
                base_hash,
                "fidelity field must affect the state hash"
            );
        }
    }

    /// extra_headers order is load-bearing (#566): the same pairs in a
    /// different order must hash differently.
    #[test]
    fn extra_headers_order_affects_hash() {
        let base = sample_state();
        let one = sample_state()
            .with_change_id(base.change_id)
            .with_logical_change_id(base.logical_change_id());
        let mut one = one.with_extra_headers(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        one.created_at = base.created_at;

        let two = sample_state()
            .with_change_id(base.change_id)
            .with_logical_change_id(base.logical_change_id());
        let mut two = two.with_extra_headers(vec![
            (b"b".to_vec(), b"2".to_vec()),
            (b"a".to_vec(), b"1".to_vec()),
        ]);
        two.created_at = base.created_at;

        assert_ne!(one.hash(), two.hash());
    }

    /// The fidelity fields set together produce a stable, recomputable
    /// hash (guards against a `hash_len`/`update_hash` divergence making
    /// the cached hash differ from a fresh `compute_hash`).
    #[test]
    fn fidelity_fields_hash_is_stable() {
        let mut state = sample_state()
            .with_committer(Principal::new("Dave", "dave@example.com"))
            .with_tz_offsets(3600, 0)
            .with_authored_at(Utc::now())
            .with_raw_message("body\n")
            .with_extra_headers(vec![
                (b"gpgsig".to_vec(), b"sig".to_vec()),
                (b"k".to_vec(), b"v".to_vec()),
            ]);
        assert_eq!(state.hash(), state.compute_hash());
    }

    /// A non-UTF8 git message body (latin-1 `café` = `caf\xe9`) must be
    /// stored byte-identically. `raw_message` is `Vec<u8>`, not `String`,
    /// precisely so these bytes survive; the hash stays stable/recomputable
    /// over the raw bytes (length-prefixed framing, NUL-safe). #564 step 1.
    #[test]
    fn non_utf8_raw_message_is_byte_preserved() {
        let raw = b"caf\xe9\n".to_vec();
        assert!(
            String::from_utf8(raw.clone()).is_err(),
            "test fixture must be invalid UTF-8 to be meaningful"
        );
        let mut state = sample_state().with_raw_message(&raw);
        assert_eq!(
            state.raw_message.as_deref(),
            Some(raw.as_slice()),
            "raw bytes preserved verbatim"
        );
        // rmp serialize → deserialize (the store's on-disk codec) keeps the
        // bytes intact, and the hash recomputes identically afterwards.
        let bytes = rmp_serde::to_vec(&state).expect("serialize state");
        let back: State = rmp_serde::from_slice(&bytes).expect("deserialize state");
        assert_eq!(back.raw_message.as_deref(), Some(raw.as_slice()));
        let mut back = back;
        assert_eq!(state.hash(), back.hash());
        assert_eq!(back.hash(), back.compute_hash());
    }

    /// A NUL byte inside the message must not be swallowed/truncated by the
    /// hash framing — length-prefixed `raw_message` is what makes this safe,
    /// where the old NUL-terminated string framing would have been ambiguous.
    #[test]
    fn raw_message_with_nul_byte_changes_hash() {
        let base = sample_state();
        let with_nul = sample_state()
            .with_change_id(base.change_id)
            .with_logical_change_id(base.logical_change_id());
        let mut a = with_nul.with_raw_message(b"a\x00b");
        a.created_at = base.created_at;

        let other = sample_state()
            .with_change_id(base.change_id)
            .with_logical_change_id(base.logical_change_id());
        let mut b = other.with_raw_message(b"a\x00c");
        b.created_at = base.created_at;

        assert_ne!(a.hash(), b.hash());
    }

    /// Close-the-class conformance: extension headers are captured from the
    /// raw commit header block in their EXACT on-the-wire order, regardless of
    /// which ones a decoder would surface as typed fields. A commit whose
    /// optional headers are in non-canonical order — `x-custom`, then a folded
    /// `gpgsig`, then `encoding`, then a folded `mergetag` — must reproduce that
    /// exact ordered `(name, value)` byte sequence. This fails if any header is
    /// reordered, prepended, appended, or dropped. #564 de-lossy step 1.
    #[test]
    fn parse_extension_headers_preserves_noncanonical_wire_order() {
        // A folded `mergetag` value carries a full tag object, which itself has
        // an internal blank line between the tag headers and the tag message —
        // on the wire that blank line is folded to a single space (` `), NEVER
        // an empty line, so it must not be mistaken for the header/body split.
        // Built line-by-line (NOT a `\`-continued literal, which would eat the
        // load-bearing leading space on each folded continuation line).
        let lines: &[&[u8]] = &[
            b"tree 1111111111111111111111111111111111111111",
            b"parent 2222222222222222222222222222222222222222",
            b"author Alice <alice@example.com> 1700000000 +0000",
            b"committer Bob <bob@example.com> 1700000100 +0000",
            b"x-custom custom value",
            b"gpgsig -----BEGIN PGP SIGNATURE-----",
            b" sig-line-1",
            b" -----END PGP SIGNATURE-----",
            b"encoding ISO-8859-1",
            b"mergetag object 3333333333333333333333333333333333333333",
            b" type commit",
            b" tag sidetag",
            b" tagger Carol <carol@example.com> 1700000050 +0000",
            b" ", // folded blank line inside the tag object (one space)
            b" signed side tag",
            b"", // the real header/body separator (empty line)
            b"the commit message",
            b"",
        ];
        let content = lines.join(&b'\n');

        let headers = parse_commit_extension_headers(&content);

        let expected: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"x-custom".to_vec(), b"custom value".to_vec()),
            (
                b"gpgsig".to_vec(),
                // Unfolded: internal newlines restored, NO trailing newline (the
                // serializer re-folds each `\n` to `\n `, spike §2).
                b"-----BEGIN PGP SIGNATURE-----\nsig-line-1\n-----END PGP SIGNATURE-----"
                    .to_vec(),
            ),
            (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
            (
                b"mergetag".to_vec(),
                // The folded ` \n` blank line unfolds to an empty segment, so the
                // tag object's header/message split survives as a real `\n\n`.
                b"object 3333333333333333333333333333333333333333\ntype commit\ntag sidetag\ntagger Carol <carol@example.com> 1700000050 +0000\n\nsigned side tag".to_vec(),
            ),
        ];

        assert_eq!(headers, expected);
    }

    /// A commit with no extension headers (the common case) yields an empty
    /// vec — `tree`/`parent`/`author`/`committer` are modelled natively and
    /// never leak into `extra_headers`.
    #[test]
    fn parse_extension_headers_empty_when_only_core_headers() {
        let content: &[u8] = b"\
tree 1111111111111111111111111111111111111111\n\
author Alice <alice@example.com> 1700000000 +0000\n\
committer Bob <bob@example.com> 1700000100 +0000\n\
\n\
just a message\n";
        assert!(parse_commit_extension_headers(content).is_empty());
    }
}
