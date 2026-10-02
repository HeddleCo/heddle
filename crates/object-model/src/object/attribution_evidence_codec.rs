// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::object::{Agent, Blob, ContentHash};

type Result<T> = std::result::Result<T, AttributionEvidenceError>;

impl AttributionEvidenceV1 {
    /// Validate only structured identity claims. Values are not an attestation.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != 1 {
            return Err(AttributionEvidenceError::UnsupportedVersion);
        }
        validate_identity(
            &self.harness,
            &self.harness_version,
            self.harness_version_scope,
            &self.selected,
            &self.response,
            &self.scope,
            !self.operations.is_empty(),
        )?;
        if self.operations.len() > ATTRIBUTION_MAX_OPERATIONS {
            return Err(AttributionEvidenceError::InvalidField("operations"));
        }
        for operation in &self.operations {
            operation.validate()?;
        }
        Ok(())
    }

    /// Capture identity without recursively copying operation contributors.
    pub fn operation_identity(&self) -> AttributionOperationIdentity {
        AttributionOperationIdentity {
            harness: self.harness.clone(),
            harness_version: self.harness_version.clone(),
            harness_version_scope: self.harness_version_scope,
            selected: self.selected.clone(),
            response: self.response.clone(),
            scope: self.scope.clone(),
            collection_methods: Vec::new(),
        }
    }

    /// Encode the bounded canonical HAE1 body stored as an ordinary Blob.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let body = rmp_serde::to_vec_named(self).map_err(|_| AttributionEvidenceError::Encoding)?;
        if body.len() + ATTRIBUTION_EVIDENCE_MAGIC.len() > ATTRIBUTION_EVIDENCE_MAX_BYTES {
            return Err(AttributionEvidenceError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(body.len() + ATTRIBUTION_EVIDENCE_MAGIC.len());
        bytes.extend_from_slice(ATTRIBUTION_EVIDENCE_MAGIC);
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    /// Decode strict canonical bytes. Reject unknown fields, trailing bytes,
    /// alternative map ordering, duplicate fields, and oversized bodies.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > ATTRIBUTION_EVIDENCE_MAX_BYTES {
            return Err(AttributionEvidenceError::TooLarge);
        }
        let body = bytes
            .strip_prefix(ATTRIBUTION_EVIDENCE_MAGIC)
            .ok_or(AttributionEvidenceError::UnsupportedVersion)?;
        let value: Self =
            rmp_serde::from_slice(body).map_err(|_| AttributionEvidenceError::Encoding)?;
        if value.to_bytes()? != bytes {
            return Err(AttributionEvidenceError::NonCanonical);
        }
        Ok(value)
    }

    /// Build the ordinary content-addressed Blob committed by a State.
    pub fn to_blob(&self) -> Result<Blob> {
        self.to_bytes().map(Blob::new)
    }

    /// Decode an already integrity-checked Blob.
    pub fn from_blob(blob: &Blob) -> Result<Self> {
        Self::from_bytes(blob.content())
    }

    /// Verify a State's exact evidence reference before parsing its claims.
    pub fn from_blob_with_hash(blob: &Blob, expected: ContentHash) -> Result<Self> {
        if blob.size() > ATTRIBUTION_EVIDENCE_MAX_BYTES {
            return Err(AttributionEvidenceError::TooLarge);
        }
        if blob.hash() != expected {
            return Err(AttributionEvidenceError::HashMismatch);
        }
        Self::from_blob(blob)
    }

    /// Legacy capture-context projection, absent for contributors or marked
    /// incomplete coverage. Complete response identity wins over selection;
    /// never synthesize a pair from two different observation scopes.
    pub fn legacy_agent(&self) -> Option<Agent> {
        // A capture can contain several producing actors or unresolved work.
        // Its capture-time identity cannot stand in for those contributors.
        if !self.operations.is_empty() || self.operations_incomplete {
            return None;
        }
        let identity = if self.response.provider.is_some() && self.response.model.is_some() {
            &self.response
        } else {
            &self.selected
        };
        let mut agent = Agent::new(
            identity.provider.as_ref()?.value.clone(),
            identity.model.as_ref()?.value.clone(),
        );
        if let (Some(session), Some(segment)) =
            (&self.scope.heddle_session_id, &self.scope.heddle_segment_id)
        {
            agent = agent.with_session(session, segment);
        }
        agent.thought_level = identity
            .thought_level
            .as_ref()
            .map(|claim| claim.value.clone());
        // The legacy slot was overloaded across harnesses. Preserve that view
        // without conflating the two namespaces in the committed rich record.
        agent.parent = self
            .scope
            .parent_actor_id
            .clone()
            .or_else(|| self.scope.parent_harness_session_id.clone());
        Some(agent)
    }

    /// Validate a capture's legacy projection before publishing its State.
    pub fn validate_attribution(
        &self,
        attribution: &crate::object::Attribution,
    ) -> crate::error::Result<()> {
        self.validate_legacy_agent(attribution.agent.as_ref())
            .map_err(|error| crate::error::HeddleError::InvalidObject(error.to_string()))
    }

    /// Reject contradictory compatibility projections. Policy is independently
    /// supplied by capture; it is not a field of this identity-only record.
    pub fn validate_legacy_agent(&self, agent: Option<&Agent>) -> Result<()> {
        self.validate()?;
        let mut actual = agent.cloned();
        if let Some(actual) = &mut actual {
            actual.policy_id = None;
        }
        if actual != self.legacy_agent() {
            return Err(AttributionEvidenceError::LegacyContradiction);
        }
        Ok(())
    }
}

impl AttributionOperationIdentity {
    pub fn validate(&self) -> Result<()> {
        if self.collection_methods.len() > ATTRIBUTION_MAX_COLLECTION_METHODS
            || self
                .collection_methods
                .iter()
                .enumerate()
                .any(|(index, method)| self.collection_methods[..index].contains(method))
        {
            return Err(AttributionEvidenceError::InvalidField("collection_methods"));
        }
        validate_identity(
            &self.harness,
            &self.harness_version,
            self.harness_version_scope,
            &self.selected,
            &self.response,
            &self.scope,
            false,
        )
    }
}

impl AttributionOperation {
    pub fn validate(&self) -> Result<()> {
        self.identity.validate()?;
        if self.changes.len() > ATTRIBUTION_MAX_FILE_CHANGES {
            return Err(AttributionEvidenceError::InvalidField("operation_changes"));
        }
        let bound = self.resolution == AttributionOperationResolution::ContentBound;
        if bound && (self.identity.scope.tool_call_id.is_none() || self.changes.is_empty()) {
            return Err(AttributionEvidenceError::InvalidField(
                "content_bound_operation",
            ));
        }
        let mut paths = std::collections::BTreeSet::new();
        for change in &self.changes {
            change.validate()?;
            if !paths.insert(change.path.as_str()) {
                return Err(AttributionEvidenceError::InvalidField(
                    "duplicate_operation_path",
                ));
            }
            if bound && change.before == change.after {
                return Err(AttributionEvidenceError::InvalidField(
                    "content_bound_transition",
                ));
            }
        }
        Ok(())
    }
}

impl AttributionFileChange {
    pub fn validate(&self) -> Result<()> {
        let path = &self.path;
        if path.is_empty()
            || path.len() > ATTRIBUTION_PATH_MAX_BYTES
            || path.contains('\\')
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || path
                .split('/')
                .next()
                .is_some_and(|part| part.contains(':'))
        {
            return Err(AttributionEvidenceError::InvalidField("operation_path"));
        }
        Ok(())
    }
}

fn validate_identity(
    harness: &Option<AttributionClaim>,
    harness_version: &Option<AttributionClaim>,
    harness_version_scope: Option<HarnessVersionScope>,
    selected: &ModelAttribution,
    response: &ModelAttribution,
    scope: &AttributionScope,
    allow_empty: bool,
) -> Result<()> {
    claim(harness, "harness")?;
    claim(harness_version, "harness_version")?;
    if harness_version.is_some() != harness_version_scope.is_some()
        || (harness_version.is_some() && harness.is_none())
    {
        return Err(AttributionEvidenceError::InvalidField(
            "harness_version_scope",
        ));
    }
    model(selected, false)?;
    model(response, true)?;
    for (name, value) in [
        ("heddle_session_id", &scope.heddle_session_id),
        ("heddle_segment_id", &scope.heddle_segment_id),
        ("harness_instance_id", &scope.harness_instance_id),
        ("harness_session_id", &scope.harness_session_id),
        ("actor_id", &scope.actor_id),
        ("parent_actor_id", &scope.parent_actor_id),
        (
            "parent_harness_session_id",
            &scope.parent_harness_session_id,
        ),
        ("turn_id", &scope.turn_id),
        ("request_id", &scope.request_id),
        ("response_id", &scope.response_id),
        ("attempt_id", &scope.attempt_id),
        ("message_id", &scope.message_id),
        ("root_turn_id", &scope.root_turn_id),
        ("tool_call_id", &scope.tool_call_id),
    ] {
        if let Some(value) = value {
            text(value, name)?;
        }
    }
    if scope.heddle_segment_id.is_some() && scope.heddle_session_id.is_none() {
        return Err(AttributionEvidenceError::InvalidField("heddle_segment_id"));
    }
    if scope.actor_id.is_some() && scope.actor_id == scope.parent_actor_id {
        return Err(AttributionEvidenceError::InvalidField("parent_actor_id"));
    }
    // Claude subagents share the root native session; their separate
    // actor id identifies the child even when both session labels match.
    if (scope.actor_id.is_none() || scope.actor_id == scope.harness_session_id)
        && scope.harness_session_id.is_some()
        && scope.harness_session_id == scope.parent_harness_session_id
    {
        return Err(AttributionEvidenceError::InvalidField(
            "parent_harness_session_id",
        ));
    }
    if !allow_empty
        && harness.is_none()
        && selected.model.is_none()
        && response.model.is_none()
        && scope.harness_session_id.is_none()
        && scope.harness_instance_id.is_none()
        && scope.actor_id.is_none()
    {
        return Err(AttributionEvidenceError::Empty);
    }
    Ok(())
}

fn model(value: &ModelAttribution, response: bool) -> Result<()> {
    for (name, field) in [
        ("provider", &value.provider),
        ("model", &value.model),
        ("version", &value.version),
        ("thought_level", &value.thought_level),
    ] {
        claim(field, name)?;
        if let Some(field) = field
            && response != (field.basis == AttributionBasis::ResponseReported)
        {
            return Err(AttributionEvidenceError::InvalidField("model_claim_basis"));
        }
    }
    if value.version.is_some() && value.model.is_none() {
        return Err(AttributionEvidenceError::InvalidField("model_version"));
    }
    Ok(())
}

fn claim(value: &Option<AttributionClaim>, name: &'static str) -> Result<()> {
    if let Some(value) = value {
        text(&value.value, name)?;
        if let Some(id) = &value.observation_id {
            text(id, "observation_id")?;
        }
    }
    Ok(())
}

// Identifiers, not prose/URLs/paths/assignments. This grammar is an admission
// bound, not a general secret detector: adapters must still copy only typed
// identity fields, never arbitrary source payloads.
fn text(value: &str, field: &'static str) -> Result<()> {
    let credential_prefix = [
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "github_pat_",
        "AKIA",
        "ASIA",
        "AIza",
    ]
    .iter()
    .any(|prefix| value.starts_with(prefix));
    if value.is_empty()
        || value.len() > ATTRIBUTION_VALUE_MAX_BYTES
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || value.eq_ignore_ascii_case("unknown")
        || value.contains("://")
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'.' | b'_' | b'-' | b'/' | b':' | b'@' | b'+' | b'[' | b']'
                )
        })
        || credential_prefix
    {
        return Err(AttributionEvidenceError::InvalidField(field));
    }
    Ok(())
}
