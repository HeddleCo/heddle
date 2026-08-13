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
                "6c4929bfebf65a906406b48957440c591eb8c4f0f7306aea37aca01016c7c256",
            ),
            (
                "open_state",
                "3733767e55beab34c5add4fa6ad514846320151cab84b8c077ee31456db14e94",
            ),
            (
                "open_change",
                "0504854868da192b823626573c041b53394217db9a1adb3eb290e913e2471c29",
            ),
            (
                "open_path",
                "39e55ce36dbc40d6bcad094825366eb962fe5e6170bf7a8a53b0a972679cc4ae",
            ),
            (
                "open_symbol",
                "a3ec2abb9288b42ab57cb2b1095ebb4b87fcf51bce6e7d57ec60838908fb81e4",
            ),
            (
                "append_turn",
                "b542d7f781fed9266dd557a8a422af1f3fce98b4fd834c0e88cc867787e32d1f",
            ),
            (
                "resolve_state",
                "7646fc4ed8e8975805491f7760c652514191bd07b7b15eabdcf6b5c8f068439f",
            ),
            (
                "resolve_change",
                "e5021f4da168fef2bb26297c6fe554cc545a3a3b7dd5a056a23d0de2bdda38b0",
            ),
            (
                "resolve_dismissed",
                "e81909c0875c57ac3920109d2291aec767595952f883b4ca4390e0af61bce9f3",
            ),
            (
                "resolve_annotation",
                "9ca40f41cc72bbf6208a22f9dcbfeaa3773d1669366864b704e2251fd012bb39",
            ),
            (
                "reopen",
                "2b997fdd5a1011255a4b85aa4f3cca3f2d0f97b2ce35d7cdcbb76a5349eeffd0",
            ),
            (
                "resolve_conflict",
                "01e63b8dbb33f11e1fa8b630e045f73030ff99db66a628fe13e84d5f9e9007b2",
            ),
            (
                "legacy_open",
                "ada646a3ab1feb54cb0b70a682079a8af0a603930f73ae0716cb861107fc4af3",
            ),
            (
                "legacy_state",
                "83a69502c2aa2d32606c2d1d5b568ddc4c555933960921e4a4f8b2e375a70e66",
            ),
            (
                "legacy_dismissed",
                "69cb8998b89d44d77e9851d2eaa619dd550ecf78e6212b07b18c3c1fd5d47989",
            ),
            (
                "legacy_annotation",
                "368a60c692b5e50bfb542f5654eccc3c62a4f65442e7404c48734386da141d32",
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
