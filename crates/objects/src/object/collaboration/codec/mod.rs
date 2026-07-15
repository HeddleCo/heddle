// SPDX-License-Identifier: Apache-2.0

mod v1;

use serde::Deserialize;

use super::{CollabOpId, CollaborationOperationEnvelope};

#[derive(Debug, thiserror::Error)]
pub enum CollaborationCodecError {
    #[error("collaboration operation encoding failed: {0}")]
    Encoding(String),
    #[error("collaboration operation decoding failed: {0}")]
    Decoding(String),
    #[error("unsupported collaboration operation version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid collaboration operation: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedCollaborationOperation {
    pub operation_id: CollabOpId,
    pub operation: CollaborationOperationEnvelope,
}

#[derive(Deserialize)]
struct VersionProbe {
    schema_version: u16,
}

pub(crate) fn encode(
    operation: &CollaborationOperationEnvelope,
) -> Result<Vec<u8>, CollaborationCodecError> {
    operation.validate()?;
    v1::encode(operation)
}

pub(crate) fn decode(
    bytes: &[u8],
) -> Result<DecodedCollaborationOperation, CollaborationCodecError> {
    let probe: VersionProbe = rmp_serde::from_slice(bytes)
        .map_err(|error| CollaborationCodecError::Decoding(error.to_string()))?;
    if probe.schema_version != 1 {
        return Err(CollaborationCodecError::UnsupportedVersion(
            probe.schema_version,
        ));
    }
    let operation = v1::decode(bytes)?;
    operation.validate()?;
    Ok(DecodedCollaborationOperation {
        operation_id: CollabOpId::for_bytes(bytes),
        operation,
    })
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;
    use crate::object::{
        Attribution, ChangeId, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationResolution, ContentHash, DiscussionRecordId,
        DiscussionTurnV1, LegacyDiscussionId, LegacyDiscussionResolutionV1, LegacySourceLocator,
        Principal, StateAttachmentId, StateId, VisibilityTier,
    };

    #[derive(Serialize)]
    struct Unsupported<'a> {
        schema_version: u16,
        body: &'a [u8],
    }

    #[test]
    fn unsupported_version_is_rejected_before_body_decode() {
        let bytes = rmp_serde::to_vec_named(&Unsupported {
            schema_version: 2,
            body: &[0xc1],
        })
        .unwrap();
        assert!(matches!(
            decode(&bytes),
            Err(CollaborationCodecError::UnsupportedVersion(2))
        ));
    }

    fn golden_operation(name: &str, body: CollaborationOperationBodyV1) -> (String, Vec<u8>) {
        let root = matches!(
            body,
            CollaborationOperationBodyV1::Open { .. }
                | CollaborationOperationBodyV1::LegacyImported { .. }
        );
        let operation = CollaborationOperationEnvelope::new(
            "disc-018f47ea-4a54-7c89-b012-3456789abcde"
                .parse::<DiscussionRecordId>()
                .unwrap(),
            if root {
                Vec::new()
            } else if matches!(body, CollaborationOperationBodyV1::ResolveConflict { .. }) {
                vec![
                    CollabOpId::from_bytes([7; 32]),
                    CollabOpId::from_bytes([8; 32]),
                ]
            } else {
                vec![CollabOpId::from_bytes([7; 32])]
            },
            CollaborationIdempotencyKey::new("k").unwrap(),
            Attribution::human(Principal::new("A", "a@b")),
            0,
            body,
        )
        .unwrap();
        (name.to_string(), operation.encode().unwrap())
    }

    fn golden_vectors() -> Vec<(String, Vec<u8>)> {
        let state = StateId::from_bytes([1; 32]);
        let change = ChangeId::from_bytes([2; 16]);
        let turn = || DiscussionTurnV1::new("x").unwrap();
        let open = |anchor| CollaborationOperationBodyV1::Open {
            title: "t".to_string(),
            anchor,
            visibility: VisibilityTier::default(),
            turn: turn(),
        };
        let locator = LegacySourceLocator::new(
            state,
            StateAttachmentId::from_hash(ContentHash::from_bytes([3; 32])),
            ContentHash::from_bytes([4; 32]),
        );
        let legacy = |resolution| CollaborationOperationBodyV1::LegacyImported {
            source: locator.clone(),
            legacy_discussion_id: LegacyDiscussionId::new("l").unwrap(),
            aliases: vec![LegacySourceLocator::new(
                StateId::from_bytes([5; 32]),
                StateAttachmentId::from_hash(ContentHash::from_bytes([6; 32])),
                ContentHash::from_bytes([7; 32]),
            )],
            title: "t".to_string(),
            anchor: CollaborationAnchor::Symbol {
                state_id: state,
                path: "p".to_string(),
                symbol: "s".to_string(),
            },
            visibility: VisibilityTier::default(),
            turns: vec![turn()],
            resolution,
        };
        vec![
            golden_operation("open_repository", open(CollaborationAnchor::Repository)),
            golden_operation(
                "open_state",
                open(CollaborationAnchor::State { state_id: state }),
            ),
            golden_operation(
                "open_change",
                open(CollaborationAnchor::Change { change_id: change }),
            ),
            golden_operation(
                "open_path",
                open(CollaborationAnchor::Path {
                    state_id: state,
                    path: "p".to_string(),
                }),
            ),
            golden_operation(
                "open_symbol",
                open(CollaborationAnchor::Symbol {
                    state_id: state,
                    path: "p".to_string(),
                    symbol: "s".to_string(),
                }),
            ),
            golden_operation(
                "append_turn",
                CollaborationOperationBodyV1::AppendTurn { turn: turn() },
            ),
            golden_operation(
                "resolve_state",
                CollaborationOperationBodyV1::Resolve {
                    resolution: CollaborationResolution::AddressedByState { state_id: state },
                },
            ),
            golden_operation(
                "resolve_change",
                CollaborationOperationBodyV1::Resolve {
                    resolution: CollaborationResolution::AddressedByChange { change_id: change },
                },
            ),
            golden_operation(
                "resolve_dismissed",
                CollaborationOperationBodyV1::Resolve {
                    resolution: CollaborationResolution::Dismissed {
                        reason: "r".to_string(),
                    },
                },
            ),
            golden_operation(
                "resolve_annotation",
                CollaborationOperationBodyV1::Resolve {
                    resolution: CollaborationResolution::Annotation {
                        annotation_id: "a".to_string(),
                    },
                },
            ),
            golden_operation(
                "reopen",
                CollaborationOperationBodyV1::Reopen {
                    reason: "r".to_string(),
                },
            ),
            golden_operation(
                "resolve_conflict",
                CollaborationOperationBodyV1::ResolveConflict {
                    competing: vec![
                        CollabOpId::from_bytes([7; 32]),
                        CollabOpId::from_bytes([8; 32]),
                    ],
                    selected: CollabOpId::from_bytes([7; 32]),
                },
            ),
            golden_operation("legacy_open", legacy(LegacyDiscussionResolutionV1::Open)),
            golden_operation(
                "legacy_state",
                legacy(LegacyDiscussionResolutionV1::AddressedByState { state_id: state }),
            ),
            golden_operation(
                "legacy_dismissed",
                legacy(LegacyDiscussionResolutionV1::Dismissed {
                    reason: "r".to_string(),
                }),
            ),
            golden_operation(
                "legacy_annotation",
                legacy(LegacyDiscussionResolutionV1::Annotation {
                    annotation_id: "a".to_string(),
                }),
            ),
        ]
    }

    #[test]
    fn v1_full_variant_msgpack_vectors_are_frozen() {
        let expected = [
            (
                "open_repository",
                "e7c3f3e61da7272d9dd32bde3c25c3db0d0aa7259ec17eb3aa0af78d4d7954d4",
            ),
            (
                "open_state",
                "478b51c28064b6c8085bfd7e81d70cb2824fd0aca889bd0ee7d84b59bf7a9caa",
            ),
            (
                "open_change",
                "18129f50b4ef581b050b17c6e7e1e1328b1e698249884f7ad02c669ced662672",
            ),
            (
                "open_path",
                "e3f332e030f3c472d0777ea308a22bcbbeb57b0c620d5395448046674248638c",
            ),
            (
                "open_symbol",
                "f68b0bd1ffd107ca9ace80ac8520c988fa92d68c681a6d8748e9231af98b9e0c",
            ),
            (
                "append_turn",
                "1a1fce360b5ab50ab74c7b58790820f46a7f153b79cf6b00b57b31d57797afb5",
            ),
            (
                "resolve_state",
                "689f69143df9e0513b41b5d6a53c47ec610b1e75ceac41a7a6553690616a0ba2",
            ),
            (
                "resolve_change",
                "a1158654efb9d351b9ff56d74c59e5b4c1c9705afc9c704621224ba196892701",
            ),
            (
                "resolve_dismissed",
                "da1a17112cf975035e2ec8ac124648926937600039bba6f117b85ead73f6f510",
            ),
            (
                "resolve_annotation",
                "5bfbaec8982a10865950109c4357d80099fde41afff07d75df7a31b595636f40",
            ),
            (
                "reopen",
                "79386d06f7b7ef6f7ae47f9c4461674dc435121f912e1e0fca6f664c1c951f25",
            ),
            (
                "resolve_conflict",
                "c3e3a161fa47a83e06873135cc8067478a8e4e2a00d6c39fd2b1b35659ebed73",
            ),
            (
                "legacy_open",
                "4428a385ca518c899d0dc3b34241d06b4dc6a1d327bb50c586c3b74890c90904",
            ),
            (
                "legacy_state",
                "3053742d3d3cdf30ae2586f5bc615bcf26b44539693141f04974d0babd951745",
            ),
            (
                "legacy_dismissed",
                "6280ef6b44c8eb27a57bb0ea9d72b220c6223505d56fb2161999dde3be964ed6",
            ),
            (
                "legacy_annotation",
                "2e184a2ab41d4a7762808f7ec4e917ec474e667c048e9a1bfdaea9677e9b18f0",
            ),
        ];
        let actual = golden_vectors()
            .into_iter()
            .map(|(name, bytes)| {
                let decoded = CollaborationOperationEnvelope::decode(&bytes).unwrap();
                assert_eq!(decoded.operation_id, CollabOpId::for_bytes(&bytes));
                (name, ContentHash::compute(&bytes).to_hex())
            })
            .collect::<Vec<_>>();
        assert_eq!(actual.len(), expected.len());
        for ((actual_name, actual_hash), (expected_name, expected_hash)) in
            actual.iter().zip(expected)
        {
            assert_eq!(actual_name, expected_name);
            assert_eq!(actual_hash, expected_hash);
        }
    }
}
