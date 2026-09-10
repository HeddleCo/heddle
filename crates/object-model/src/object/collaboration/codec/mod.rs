// SPDX-License-Identifier: Apache-2.0

mod v2;

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
    v2::encode(operation)
}

pub(crate) fn decode(
    bytes: &[u8],
) -> Result<DecodedCollaborationOperation, CollaborationCodecError> {
    let probe: VersionProbe = rmp_serde::from_slice(bytes)
        .map_err(|error| CollaborationCodecError::Decoding(error.to_string()))?;
    if probe.schema_version != super::COLLABORATION_OPERATION_SCHEMA_VERSION {
        return Err(CollaborationCodecError::UnsupportedVersion(
            probe.schema_version,
        ));
    }
    let operation = v2::decode(bytes)?;
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
        AnnotationKind, Attribution, ChangeId, CollaborationAnchor, CollaborationAnchorStatus,
        CollaborationIdempotencyKey, CollaborationOperationBodyV1, CollaborationResolution,
        ContentHash, DiscussionRecordId, DiscussionTurnV1, LegacyDiscussionId,
        LegacyDiscussionResolutionV1, LegacySourceLocator, Principal, StateAttachmentId, StateId,
        VisibilityTier,
    };

    #[derive(Serialize)]
    struct Unsupported<'a> {
        schema_version: u16,
        body: &'a [u8],
    }

    #[test]
    fn unsupported_version_is_rejected_before_body_decode() {
        let bytes = rmp_serde::to_vec_named(&Unsupported {
            schema_version: 3,
            body: &[0xc1],
        })
        .unwrap();
        assert!(matches!(
            decode(&bytes),
            Err(CollaborationCodecError::UnsupportedVersion(3))
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
            blocking: false,
            title: "t".to_string(),
            anchor,
            visibility: VisibilityTier::default(),
            turn: turn(),
            thread_ref: None,
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
                "rebind_anchor",
                CollaborationOperationBodyV1::RebindAnchor {
                    anchor: CollaborationAnchor::Symbol {
                        state_id: state,
                        path: "p".to_string(),
                        symbol: "s2".to_string(),
                    },
                    status: CollaborationAnchorStatus::Moved,
                    body_changed_since_open: true,
                },
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
                "resolve_into_annotation",
                CollaborationOperationBodyV1::Resolve {
                    resolution: CollaborationResolution::IntoAnnotation {
                        annotation_kind: AnnotationKind::Rationale,
                        content: "why".to_string(),
                        tags: vec!["design".to_string()],
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
    fn v2_full_variant_msgpack_vectors_are_frozen() {
        let expected = [
            (
                "open_repository",
                "69b3baebacb9d29c3d6c5cac73d6f14b2fe5231e061065249e808e2d92cddefe",
            ),
            (
                "open_state",
                "785cf6234fdfc4a01ff068a9b67a2c0d60172f59795c39c3c5b4e1037176a964",
            ),
            (
                "open_change",
                "e520779bc52b05e753a139bddafd25dcc8765182b865cd305691f85129b58c03",
            ),
            (
                "open_path",
                "1c814db69893d0ee71abe9d5a1d7d17724ff893fdf36bf90e9c40aa4beae9803",
            ),
            (
                "open_symbol",
                "9f43eb25d8c920d89a31c148fcd177e789e909eb7d3715fc2f899f85fa680197",
            ),
            (
                "append_turn",
                "777d8164d530fe27545685f23b283765a3b912f96a262dbad836faba498a9279",
            ),
            (
                "rebind_anchor",
                "e548683d8c7f20c8550627886f196a81b91e6ae52b8cdaadbfdd8944ceda1f8d",
            ),
            (
                "resolve_state",
                "7063d23abba098f608b13f2b807892bf7a5b61cb7b8487897ba84e95ff519dc9",
            ),
            (
                "resolve_change",
                "bcaa6bd96243624dcfe25942e92f59104d08291b2daf5dfd4c8d68f6fbd98472",
            ),
            (
                "resolve_dismissed",
                "4277710d09ae7043616335695b98fc89ce15583fd90437b28587c310492fe8f0",
            ),
            (
                "resolve_annotation",
                "c4eee97f235a3b534e423f20159ddba3077f9e0079ebc0697fa6c2426b394827",
            ),
            (
                "resolve_into_annotation",
                "e1470222f139ff1e6e06480996a7c27c2ef9d162eed2405ad3d838304d94d73e",
            ),
            (
                "reopen",
                "fa59e9c396a77ce07e57cb7b312f76438c3a002a1a4f053d1da13d9d10e03f7d",
            ),
            (
                "resolve_conflict",
                "d4c76d8ff078907d528613f7811151797d204f01216876623aecd675e9466606",
            ),
            (
                "legacy_open",
                "56e355d619b3db940a4cce75d525dac493d7e925c3e37cf85e1a789c6454713b",
            ),
            (
                "legacy_state",
                "44281502243244c40fc8be8b1fa452e3fd4491a455271462cb1236bac5b8136d",
            ),
            (
                "legacy_dismissed",
                "c9dd8f6850af3fce3797bb2ce853414dee066d29821756cbb8e2c6627ed56688",
            ),
            (
                "legacy_annotation",
                "8148359eab1c913a5a1b98debc9aa9f2a455cbbbc7b376b43b80b59c0b709331",
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
