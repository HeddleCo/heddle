// SPDX-License-Identifier: Apache-2.0
//! weft#451 Tier-2 wire types for parse-free semantic graph queries.
//!
//! Heddle owns the request/response body. weft still owes the hosted RPCs:
//!
//! ```text
//! // Residual — implement in weft, do not add a parser there.
//! rpc GetSemanticRefs(SemanticGraphQueryRequest) returns (SemanticGraphQueryResponse);
//! rpc GetSemanticImporters(SemanticGraphQueryRequest) returns (SemanticGraphQueryResponse);
//! ```
//!
//! `GetSemanticRefs` sets `kind` to `refs_of` or `callers_of` and fills
//! `anchor`. `GetSemanticImporters` sets `kind` to `importers_of` and fills
//! `path`. Both are state-anchored; time-travel is the attached index, not a
//! re-parse. Salsa-style memoization is deferred.

pub use objects::object::{
    SemanticGraphQueryKind, SemanticGraphQueryRequest, SemanticGraphQueryResponse, SemanticGraphRef,
};

#[cfg(test)]
mod tests {
    use objects::object::SymbolAnchor;

    use super::*;

    #[test]
    fn weft_tier2_envelope_roundtrips() {
        let request = SemanticGraphQueryRequest {
            state_id: "hs-1".to_string(),
            kind: SemanticGraphQueryKind::RefsOf,
            anchor: Some(SymbolAnchor::new("src/api.rs", "greet")),
            path: None,
        };
        let encoded = rmp_serde::to_vec_named(&request).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<SemanticGraphQueryRequest>(&encoded).unwrap(),
            request
        );
    }
}
