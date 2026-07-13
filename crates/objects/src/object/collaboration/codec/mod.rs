// SPDX-License-Identifier: Apache-2.0

mod v1;

use serde::Deserialize;

use super::{CollabOpId, CollaborationOperationEnvelope};

#[derive(Debug, thiserror::Error)]
pub enum CollaborationCodecError {
    #[error("collaboration operation encoding failed: {0}")]
    Encoding(String),
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
        .map_err(|error| CollaborationCodecError::Encoding(error.to_string()))?;
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
}
