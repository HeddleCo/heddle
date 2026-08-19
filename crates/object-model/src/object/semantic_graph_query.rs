// SPDX-License-Identifier: Apache-2.0
//! Parse-free semantic graph query envelopes (heddle#1276).
//!
//! These types are the request/response body weft#451 Tier-2 will consume.
//! They describe already-stored edges and importer rows; they never parse.

use serde::{Deserialize, Serialize};

use super::{ByteSpan, OccurrenceRole, SemanticEdgeKind, SymbolAnchor};

/// Graph primitive selected by `heddle semantic refs` and the weft#451 wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticGraphQueryKind {
    /// Occurrences that resolve to a definition (`refs_of(state, anchor)`).
    RefsOf,
    /// Call-edge subset of [`Self::RefsOf`].
    CallersOf,
    /// Direct importers of a file (`importers_of(state, path)`).
    ImportersOf,
}

/// One persisted reference to a definition, reconstructed without parsing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticGraphRef {
    pub source_path: String,
    pub source_occurrence: u32,
    pub name: String,
    pub role: Option<OccurrenceRole>,
    pub kind: SemanticEdgeKind,
    pub span: Option<ByteSpan>,
    pub target: SymbolAnchor,
    pub target_definition: u32,
}

/// weft#451 Tier-2 request body. Hosted RPC names remain residual in weft.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticGraphQueryRequest {
    pub state_id: String,
    pub kind: SemanticGraphQueryKind,
    pub anchor: Option<SymbolAnchor>,
    pub path: Option<String>,
}

/// weft#451 Tier-2 response body. `index_present` is false when the state
/// has no attached semantic index; the query then returns empty collections
/// and never computes one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticGraphQueryResponse {
    pub state_id: String,
    pub kind: SemanticGraphQueryKind,
    pub anchor: Option<SymbolAnchor>,
    pub path: Option<String>,
    pub index_present: bool,
    pub refs: Vec<SemanticGraphRef>,
    pub importers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_envelope_roundtrips_named_json() {
        let request = SemanticGraphQueryRequest {
            state_id: "hs-1".to_string(),
            kind: SemanticGraphQueryKind::RefsOf,
            anchor: Some(SymbolAnchor::new("src/api.rs", "greet")),
            path: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"refs_of\""));
        assert_eq!(
            serde_json::from_str::<SemanticGraphQueryRequest>(&json).unwrap(),
            request
        );
    }
}
