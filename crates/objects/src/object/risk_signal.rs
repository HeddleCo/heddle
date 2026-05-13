// SPDX-License-Identifier: Apache-2.0
//! Typed risk signals computed against a state and persisted alongside it.
//!
//! Computation is pure (`(prior_state, new_state, repo_config) -> Vec<RiskSignal>`)
//! and lives in `crates/state_review/`. This module owns only the shape: what
//! a signal is, how it serializes on disk, and the validation rules.
//!
//! The full set of fired signals is stored on the state. Tick budgeting (which
//! signals to surface in the review UI) happens at render time and is never
//! baked into storage — see [`state_review::budget`].
//!
//! Wire encoding: rmp-serde MessagePack. Format version is `1`. New optional
//! fields are appended at the tail of [`RiskSignal`] with `#[serde(default)]`,
//! matching the convention used elsewhere in the object model.

use serde::{Deserialize, Serialize};

use crate::object::hash::ChangeId;

/// Maximum length of [`RiskSignal::reason`], in bytes.
///
/// The reason is meant to be a single sentence, surfaced in tight gutter UI.
/// Keeping the cap at 200 forces producers to be specific and prevents the
/// review payload from ballooning when many signals fire.
pub const MAX_REASON_LEN: usize = 200;

/// Top-level encoded blob. Stored under a [`ContentHash`] referenced from
/// [`State::risk_signals`]. A blob with `format_version > FORMAT_VERSION` is
/// rejected; older versions are read with the missing-field defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskSignalBlob {
    pub format_version: u8,
    pub signals: Vec<RiskSignal>,
}

impl RiskSignalBlob {
    pub const FORMAT_VERSION: u8 = 1;

    pub fn new(signals: Vec<RiskSignal>) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            signals,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, RiskSignalError> {
        rmp_serde::to_vec(self).map_err(|err| RiskSignalError::Encoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RiskSignalError> {
        let blob: Self = rmp_serde::from_slice(bytes)
            .map_err(|err| RiskSignalError::Encoding(err.to_string()))?;
        blob.validate()?;
        Ok(blob)
    }

    pub fn validate(&self) -> Result<(), RiskSignalError> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(RiskSignalError::UnsupportedVersion(self.format_version));
        }
        for signal in &self.signals {
            signal.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskSignal {
    pub kind: RiskSignalKind,
    pub anchor: SignalAnchor,
    pub reason: String,
    pub producer: ProducerId,
    /// Unix epoch seconds.
    pub computed_at: i64,
    /// Optional state this signal was computed against. Useful for retracing
    /// when a signal moves between renders (e.g., anchor travel after a
    /// rename).
    #[serde(default)]
    pub computed_against: Option<ChangeId>,
}

impl RiskSignal {
    pub fn validate(&self) -> Result<(), RiskSignalError> {
        if self.reason.is_empty() {
            return Err(RiskSignalError::EmptyReason);
        }
        if self.reason.len() > MAX_REASON_LEN {
            return Err(RiskSignalError::ReasonTooLong {
                len: self.reason.len(),
                max: MAX_REASON_LEN,
            });
        }
        self.anchor.validate()?;
        self.producer.validate()?;
        Ok(())
    }

    /// Stable canonical anchor string used to group signals on the same anchor
    /// during render-time budgeting. The format is intentionally simple so
    /// budgeting comparisons are cheap and order-independent.
    pub fn anchor_key(&self) -> String {
        self.anchor.canonical()
    }
}

/// Why a signal fired. Variants are wire-stable; new variants are appended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskSignalKind {
    /// New control-flow shape that doesn't appear elsewhere in the repo.
    Novelty,
    /// No test in the repo statically reaches the changed symbol.
    /// Reasoning text *must* clarify this is static reachability via
    /// tree-sitter, not runtime coverage.
    TestReachability,
    /// New code structurally diverges from local exemplars (sibling
    /// functions or the prior version of the same symbol).
    PatternDeviation,
    /// An invariant or `enforces`-tagged annotation lives on the changed
    /// symbol.
    InvariantAdjacency,
    /// Agent flagged uncertainty about its own output. Passthrough from
    /// the captured state's provenance.
    SelfFlaggedUncertainty,
}

impl RiskSignalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Novelty => "novelty",
            Self::TestReachability => "test_reachability",
            Self::PatternDeviation => "pattern_deviation",
            Self::InvariantAdjacency => "invariant_adjacency",
            Self::SelfFlaggedUncertainty => "self_flagged_uncertainty",
        }
    }

    /// Render-time priority. Lower numbers surface first when budgeting.
    /// See `state_review::budget` for the full algorithm.
    ///
    /// The order is load-bearing: changing it changes which signals reviewers
    /// see first when many fire on the same state. If you bump these numbers,
    /// update the budgeting test goldens too.
    pub fn priority_rank(&self) -> u8 {
        match self {
            Self::InvariantAdjacency => 0,
            Self::SelfFlaggedUncertainty => 1,
            Self::PatternDeviation => 2,
            Self::Novelty => 3,
            Self::TestReachability => 4,
        }
    }
}

/// Where in the change a signal fires. Symbol-level is preferred — symbols
/// are durable across renames; line ranges are computed at fire time and
/// drift as code is reformatted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalAnchor {
    pub file: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub line_range: Option<(u32, u32)>,
}

impl SignalAnchor {
    pub fn file(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            symbol: None,
            line_range: None,
        }
    }

    pub fn symbol(file: impl Into<String>, symbol: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            symbol: Some(symbol.into()),
            line_range: None,
        }
    }

    pub fn with_line_range(mut self, start: u32, end: u32) -> Self {
        self.line_range = Some((start, end));
        self
    }

    pub fn validate(&self) -> Result<(), RiskSignalError> {
        if self.file.is_empty() {
            return Err(RiskSignalError::EmptyAnchorFile);
        }
        if let Some((start, end)) = self.line_range
            && start > end
        {
            return Err(RiskSignalError::InvalidLineRange(start, end));
        }
        Ok(())
    }

    /// Stable canonical form `<file>[:symbol][:start-end]` for grouping.
    pub fn canonical(&self) -> String {
        let mut s = self.file.clone();
        if let Some(symbol) = &self.symbol {
            s.push(':');
            s.push_str(symbol);
        }
        if let Some((start, end)) = self.line_range {
            s.push(':');
            s.push_str(&format!("{start}-{end}"));
        }
        s
    }
}

/// Identifies the producer that fired this signal. The `version` lets
/// budgeting and signal-health surfaces age out signals from old producer
/// versions without re-running computation — important when we tune a
/// producer's heuristics and want to compare apples to apples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerId {
    pub module: String,
    pub version: u32,
}

impl ProducerId {
    pub fn new(module: impl Into<String>, version: u32) -> Self {
        Self {
            module: module.into(),
            version,
        }
    }

    pub fn validate(&self) -> Result<(), RiskSignalError> {
        if self.module.is_empty() {
            return Err(RiskSignalError::EmptyProducerModule);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RiskSignalError {
    #[error("unsupported risk signal blob version {0}")]
    UnsupportedVersion(u8),
    #[error("risk signal reason must not be empty")]
    EmptyReason,
    #[error("risk signal reason too long ({len} bytes, max {max})")]
    ReasonTooLong { len: usize, max: usize },
    #[error("risk signal anchor must reference a non-empty file")]
    EmptyAnchorFile,
    #[error("risk signal line range start {0} exceeds end {1}")]
    InvalidLineRange(u32, u32),
    #[error("risk signal producer module must not be empty")]
    EmptyProducerModule,
    #[error("risk signal blob encoding error: {0}")]
    Encoding(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_signal(kind: RiskSignalKind, file: &str, sym: &str) -> RiskSignal {
        RiskSignal {
            kind,
            anchor: SignalAnchor::symbol(file, sym),
            reason: "structural divergence from sibling implementations".into(),
            producer: ProducerId::new("pattern_deviation", 1),
            computed_at: 1_700_000_000,
            computed_against: None,
        }
    }

    #[test]
    fn empty_reason_is_rejected() {
        let mut sig = sample_signal(RiskSignalKind::Novelty, "src/lib.rs", "foo");
        sig.reason = String::new();
        assert!(matches!(sig.validate(), Err(RiskSignalError::EmptyReason)));
    }

    #[test]
    fn over_long_reason_is_rejected() {
        let mut sig = sample_signal(RiskSignalKind::Novelty, "src/lib.rs", "foo");
        sig.reason = "x".repeat(MAX_REASON_LEN + 1);
        assert!(matches!(
            sig.validate(),
            Err(RiskSignalError::ReasonTooLong { .. })
        ));
    }

    #[test]
    fn minimum_anchor_validates() {
        let sig = sample_signal(RiskSignalKind::TestReachability, "src/lib.rs", "bar");
        sig.validate().unwrap();
    }

    #[test]
    fn anchor_canonical_is_stable() {
        let a = SignalAnchor::symbol("src/lib.rs", "foo").with_line_range(10, 12);
        let b = SignalAnchor::symbol("src/lib.rs", "foo").with_line_range(10, 12);
        assert_eq!(a.canonical(), b.canonical());
        assert_eq!(a.canonical(), "src/lib.rs:foo:10-12");
    }

    #[test]
    fn priority_order_matches_spec() {
        assert!(
            RiskSignalKind::InvariantAdjacency.priority_rank()
                < RiskSignalKind::SelfFlaggedUncertainty.priority_rank()
        );
        assert!(
            RiskSignalKind::SelfFlaggedUncertainty.priority_rank()
                < RiskSignalKind::PatternDeviation.priority_rank()
        );
        assert!(
            RiskSignalKind::PatternDeviation.priority_rank()
                < RiskSignalKind::Novelty.priority_rank()
        );
        assert!(
            RiskSignalKind::Novelty.priority_rank()
                < RiskSignalKind::TestReachability.priority_rank()
        );
    }

    #[test]
    fn blob_encode_decode_roundtrips() {
        let blob = RiskSignalBlob::new(vec![sample_signal(
            RiskSignalKind::Novelty,
            "src/lib.rs",
            "foo",
        )]);
        let bytes = blob.encode().unwrap();
        let decoded = RiskSignalBlob::decode(&bytes).unwrap();
        assert_eq!(blob, decoded);
    }

    #[test]
    fn future_version_is_rejected() {
        let blob = RiskSignalBlob {
            format_version: RiskSignalBlob::FORMAT_VERSION + 1,
            signals: vec![],
        };
        assert!(matches!(
            blob.validate(),
            Err(RiskSignalError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn empty_producer_module_rejected() {
        let mut sig = sample_signal(RiskSignalKind::Novelty, "src/lib.rs", "foo");
        sig.producer.module = String::new();
        assert!(matches!(
            sig.validate(),
            Err(RiskSignalError::EmptyProducerModule)
        ));
    }
}