use core::convert::TryFrom;

use api::heddle::api::v1alpha1::{
    HostedGrant, HostedNamespace, HostedRepository, ObjectAvailabilityStatus, ObjectDescriptor,
    RepositoryRef, StateAttachmentKind as ProtoStateAttachmentKind, StateId as ProtoStateId,
    TransferCheckpoint, TransportMode, repository_ref::Reference,
};
use base64::Engine as _;
use cli_shared::ClientConfig;
use objects::object::{ContentHash, StateAttachmentId, StateAttachmentKind, StateId};
use wire::{ObjectId, ObjectInfo, ObjectType, ProtocolError};

use super::HostedError;

#[derive(Debug, Clone)]
pub(crate) struct HostedTransportPolicy {
    pub chunk_size: usize,
    pub max_inflight_objects: usize,
    pub resume_attempts: usize,
}

impl HostedTransportPolicy {
    pub fn from_client_config(config: &ClientConfig) -> Self {
        let chunk_size = config.chunk_size.max(1);
        let max_inflight_objects = (chunk_size / (16 * 1024)).clamp(1, 16);
        Self {
            chunk_size,
            max_inflight_objects,
            resume_attempts: 2,
        }
    }

    pub fn transfer_checkpoint_with_mode(
        &self,
        transfer_id: impl Into<String>,
        mode: TransportMode,
        chunk_index: u32,
        resume_offset: u64,
        is_complete: bool,
    ) -> TransferCheckpoint {
        TransferCheckpoint {
            transfer_id: transfer_id.into(),
            transport_mode: mode as i32,
            resume_offset,
            chunk_index,
            checkpoint: Vec::new(),
            is_complete,
        }
    }
}

/// Map a heddle [`StateAttachmentKind`] onto its proto counterpart. Exhaustive
/// by construction: adding a kind forces an arm here.
pub(super) fn attachment_kind_to_proto(kind: StateAttachmentKind) -> ProtoStateAttachmentKind {
    match kind {
        StateAttachmentKind::Context => ProtoStateAttachmentKind::Context,
        StateAttachmentKind::RiskSignals => ProtoStateAttachmentKind::RiskSignals,
        StateAttachmentKind::ReviewSignatures => ProtoStateAttachmentKind::ReviewSignatures,
        StateAttachmentKind::Discussions => ProtoStateAttachmentKind::Discussions,
        StateAttachmentKind::StructuredConflicts => ProtoStateAttachmentKind::StructuredConflicts,
        StateAttachmentKind::SemanticIndex => ProtoStateAttachmentKind::SemanticIndex,
        StateAttachmentKind::Signature => ProtoStateAttachmentKind::Signature,
    }
}

/// Map a proto attachment kind back onto its heddle counterpart. `Unspecified`
/// carries no kind (`None`) — a descriptor for an attachment MUST name a
/// concrete kind, so callers hard-error on `None`. Exhaustive, no `_ =>`.
fn attachment_kind_from_proto(kind: ProtoStateAttachmentKind) -> Option<StateAttachmentKind> {
    match kind {
        ProtoStateAttachmentKind::Unspecified => None,
        ProtoStateAttachmentKind::Context => Some(StateAttachmentKind::Context),
        ProtoStateAttachmentKind::RiskSignals => Some(StateAttachmentKind::RiskSignals),
        ProtoStateAttachmentKind::ReviewSignatures => Some(StateAttachmentKind::ReviewSignatures),
        ProtoStateAttachmentKind::Discussions => Some(StateAttachmentKind::Discussions),
        ProtoStateAttachmentKind::StructuredConflicts => {
            Some(StateAttachmentKind::StructuredConflicts)
        }
        ProtoStateAttachmentKind::SemanticIndex => Some(StateAttachmentKind::SemanticIndex),
        ProtoStateAttachmentKind::Signature => Some(StateAttachmentKind::Signature),
    }
}

pub(super) fn parse_descriptor_to_info(
    descriptor: ObjectDescriptor,
) -> Result<ObjectInfo, ProtocolError> {
    let obj_type = parse_object_type(&descriptor.object_type)?;
    // Resolve the carried attachment kind up front. For an attachment
    // descriptor this MUST be a concrete kind — an `UNSPECIFIED` (or
    // unrecognized) value is a hard error, not a silent default.
    let attachment_kind = if obj_type == ObjectType::StateAttachment {
        let proto_kind = ProtoStateAttachmentKind::try_from(descriptor.attachment_kind)
            .unwrap_or(ProtoStateAttachmentKind::Unspecified);
        Some(attachment_kind_from_proto(proto_kind).ok_or_else(|| {
            ProtocolError::InvalidState(
                "state attachment descriptor is missing attachment_kind (UNSPECIFIED)".to_string(),
            )
        })?)
    } else {
        None
    };
    let id = parse_object_id(&descriptor.id, obj_type, attachment_kind)?;
    Ok(ObjectInfo {
        id,
        obj_type,
        size: 0,
        delta_base: None,
    })
}

pub(super) fn decode_blob_content(
    content: String,
    is_binary: bool,
) -> Result<Vec<u8>, ProtocolError> {
    if is_binary {
        base64::engine::general_purpose::STANDARD
            .decode(content.as_bytes())
            .map_err(|err| ProtocolError::Serialization(err.to_string()))
    } else {
        Ok(content.into_bytes())
    }
}

pub(super) fn parse_object_id(
    value: &str,
    obj_type: ObjectType,
    attachment_kind: Option<StateAttachmentKind>,
) -> Result<ObjectId, ProtocolError> {
    match obj_type {
        // State and its per-state visibility sidecar are both keyed by StateId.
        ObjectType::State | ObjectType::StateVisibility => {
            Ok(ObjectId::StateId(StateId::parse(value).map_err(|err| {
                ProtocolError::InvalidState(err.to_string())
            })?))
        }
        ObjectType::StateAttachment => {
            let (state, attachment) = value.split_once(':').ok_or_else(|| {
                ProtocolError::InvalidState("invalid state attachment locator".to_string())
            })?;
            // An attachment ObjectId is only constructible WITH a kind; the
            // caller resolves it from the descriptor and hard-errors on
            // UNSPECIFIED before reaching here.
            let kind = attachment_kind.ok_or_else(|| {
                ProtocolError::InvalidState(
                    "state attachment descriptor is missing attachment_kind".to_string(),
                )
            })?;
            Ok(ObjectId::StateAttachment {
                state: StateId::parse(state)
                    .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                id: StateAttachmentId::from_hash(
                    ContentHash::from_hex(attachment)
                        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                ),
                kind,
            })
        }
        ObjectType::Blob | ObjectType::Tree | ObjectType::Action | ObjectType::Redaction => {
            Ok(ObjectId::Hash(ContentHash::from_hex(value).map_err(
                |err| ProtocolError::InvalidState(err.to_string()),
            )?))
        }
    }
}

pub(super) fn parse_object_type(value: &str) -> Result<ObjectType, ProtocolError> {
    ObjectType::from_wire(value)
}

pub(super) fn to_proto_object_info(info: &ObjectInfo) -> ObjectDescriptor {
    object_descriptor_with_status(info, ObjectAvailabilityStatus::Present, "")
}

pub(super) fn object_descriptor_with_status(
    info: &ObjectInfo,
    availability_status: ObjectAvailabilityStatus,
    availability_note: impl Into<String>,
) -> ObjectDescriptor {
    // Carry the attachment kind for attachment descriptors; every other
    // object type leaves it UNSPECIFIED. Kind is carried, not keyed — the
    // dedup key stays `(id, object_type)`.
    let attachment_kind = match &info.id {
        ObjectId::StateAttachment { kind, .. } => attachment_kind_to_proto(*kind),
        ObjectId::Hash(_) | ObjectId::StateId(_) => ProtoStateAttachmentKind::Unspecified,
    };
    ObjectDescriptor {
        id: match &info.id {
            ObjectId::Hash(hash) => hash.to_hex(),
            ObjectId::StateId(state_id) => state_id.to_string_full(),
            ObjectId::StateAttachment { state, id, kind: _ } => {
                format!("{}:{}", state.to_string_full(), id.as_hash().to_hex())
            }
        },
        object_type: object_type_name(info.obj_type).to_string(),
        availability_status: availability_status as i32,
        availability_note: availability_note.into(),
        attachment_kind: attachment_kind as i32,
    }
}

pub(super) fn transport_mode_name(mode: i32) -> &'static str {
    match TransportMode::try_from(mode).unwrap_or(TransportMode::Unspecified) {
        TransportMode::NativePack => "native-pack",
        TransportMode::Unspecified => "unspecified",
    }
}

pub(super) fn object_type_name(obj_type: ObjectType) -> &'static str {
    obj_type.wire_name()
}

pub(super) fn descriptor_id(descriptor: &ObjectDescriptor) -> (String, String) {
    (descriptor.id.clone(), descriptor.object_type.clone())
}

/// Compute the same `(id, object_type)` key as
/// `descriptor_id(&to_proto_object_info(info))` without the throwaway full
/// proto encode. Must stay byte-identical to the descriptor the server keys on.
pub(super) fn descriptor_id_from_info(info: &ObjectInfo) -> (String, String) {
    let id = match &info.id {
        ObjectId::Hash(hash) => hash.to_hex(),
        ObjectId::StateId(state_id) => state_id.to_string_full(),
        ObjectId::StateAttachment { state, id, kind: _ } => {
            format!("{}:{}", state.to_string_full(), id.as_hash().to_hex())
        }
    };
    (id, object_type_name(info.obj_type).to_string())
}

pub(super) fn hosted_to_protocol_error(error: HostedError) -> ProtocolError {
    use api::heddle::api::v1alpha1::CallFailureCode;
    match error {
        HostedError::Call {
            code,
            message,
            details,
        } => {
            if !details.is_empty() {
                return ProtocolError::RemoteFailure {
                    code: remote_failure_code(code),
                    message,
                    details: details.into_iter().map(remote_failure_detail).collect(),
                };
            }

            match code {
                CallFailureCode::Unauthenticated | CallFailureCode::PermissionDenied => {
                    ProtocolError::AuthorizationFailed(message)
                }
                CallFailureCode::NotFound => ProtocolError::ObjectNotFound(message),
                CallFailureCode::AlreadyExists => ProtocolError::AlreadyExists(message),
                CallFailureCode::InvalidArgument | CallFailureCode::FailedPrecondition => {
                    ProtocolError::InvalidState(message)
                }
                _ => ProtocolError::RemoteFailure {
                    code: remote_failure_code(code),
                    message,
                    details: Vec::new(),
                },
            }
        }
        HostedError::Decode(error) => ProtocolError::Serialization(error.to_string()),
        HostedError::Transport(message) => ProtocolError::Io(std::io::Error::other(message)),
        error => ProtocolError::Remote(error.to_string()),
    }
}

fn remote_failure_code(
    code: api::heddle::api::v1alpha1::CallFailureCode,
) -> wire::RemoteFailureCode {
    use api::heddle::api::v1alpha1::CallFailureCode as Api;
    use wire::RemoteFailureCode as Wire;
    match code {
        Api::Unspecified => Wire::Unspecified,
        Api::Cancelled => Wire::Cancelled,
        Api::Unknown => Wire::Unknown,
        Api::InvalidArgument => Wire::InvalidArgument,
        Api::DeadlineExceeded => Wire::DeadlineExceeded,
        Api::NotFound => Wire::NotFound,
        Api::AlreadyExists => Wire::AlreadyExists,
        Api::PermissionDenied => Wire::PermissionDenied,
        Api::ResourceExhausted => Wire::ResourceExhausted,
        Api::FailedPrecondition => Wire::FailedPrecondition,
        Api::Aborted => Wire::Aborted,
        Api::OutOfRange => Wire::OutOfRange,
        Api::Unimplemented => Wire::Unimplemented,
        Api::Internal => Wire::Internal,
        Api::Unavailable => Wire::Unavailable,
        Api::DataLoss => Wire::DataLoss,
        Api::Unauthenticated => Wire::Unauthenticated,
    }
}

fn remote_duration(value: prost_types::Duration) -> wire::RemoteDuration {
    wire::RemoteDuration {
        seconds: value.seconds,
        nanos: value.nanos,
    }
}

fn remote_cursor(value: api::heddle::api::v1alpha1::CursorFailure) -> wire::RemoteCursorFailure {
    use api::heddle::api::v1alpha1::cursor_failure::Reason as Api;
    use wire::RemoteCursorReason as Wire;
    let reason = match value.reason() {
        Api::Unspecified => Wire::Unspecified,
        Api::Stale => Wire::Stale,
        Api::Expired => Wire::Expired,
    };
    wire::RemoteCursorFailure {
        reason,
        expired_at: value.expired_at.map(|timestamp| wire::RemoteTimestamp {
            seconds: timestamp.seconds,
            nanos: timestamp.nanos,
        }),
        restart_cursor: value.restart_cursor,
    }
}

fn remote_failure_detail(detail: prost_types::Any) -> wire::RemoteFailureDetail {
    use api::heddle::api::v1alpha1::{
        CapabilityRequirement, ConflictDetail, CursorFailure, PolicyDenial, RetryAdvice,
        StreamFailure,
    };
    use prost::Message as _;

    let message_name = detail.type_url.rsplit('/').next().unwrap_or_default();
    match message_name {
        "heddle.api.v1alpha1.RetryAdvice" => {
            RetryAdvice::decode(detail.value.as_slice()).map(|value| {
                wire::RemoteFailureDetail::Retry {
                    retry_after: value.retry_after.map(remote_duration),
                }
            })
        }
        "heddle.api.v1alpha1.ConflictDetail" => ConflictDetail::decode(detail.value.as_slice())
            .map(|value| wire::RemoteFailureDetail::Conflict {
                resource: value.resource,
                expected_version: value.expected_version,
                actual_version: value.actual_version,
            }),
        "heddle.api.v1alpha1.CursorFailure" => CursorFailure::decode(detail.value.as_slice())
            .map(|value| wire::RemoteFailureDetail::Cursor(remote_cursor(value))),
        "heddle.api.v1alpha1.CapabilityRequirement" => {
            CapabilityRequirement::decode(detail.value.as_slice()).map(|value| {
                wire::RemoteFailureDetail::CapabilityRequirement {
                    capabilities: value.capabilities,
                }
            })
        }
        "heddle.api.v1alpha1.PolicyDenial" => {
            PolicyDenial::decode(detail.value.as_slice()).map(|value| {
                wire::RemoteFailureDetail::PolicyDenial {
                    policy_id: value.policy_id,
                    rule: value.rule,
                    human_verification_can_override: value.human_verification_can_override,
                }
            })
        }
        "heddle.api.v1alpha1.StreamFailure" => {
            StreamFailure::decode(detail.value.as_slice()).map(|value| {
                wire::RemoteFailureDetail::Stream {
                    code: remote_failure_code(value.code()),
                    message: value.message,
                    retry_after: value
                        .retry
                        .and_then(|retry| retry.retry_after)
                        .map(remote_duration),
                    cursor: value.cursor.map(remote_cursor),
                }
            })
        }
        _ => {
            return wire::RemoteFailureDetail::Unknown {
                type_url: detail.type_url,
                value: detail.value,
            };
        }
    }
    .unwrap_or(wire::RemoteFailureDetail::Unknown {
        type_url: detail.type_url,
        value: detail.value,
    })
}

pub(super) fn repository_ref(path: &str) -> Option<RepositoryRef> {
    Some(RepositoryRef {
        reference: Some(Reference::CanonicalPath(path.to_string())),
    })
}

pub(crate) fn repository_ref_path(repository: &RepositoryRef) -> Option<&str> {
    match repository.reference.as_ref() {
        Some(Reference::HostedId(id) | Reference::CanonicalPath(id)) if !id.is_empty() => Some(id),
        None => None,
        _ => None,
    }
}

pub(super) fn proto_state_id(state_id: StateId) -> Option<ProtoStateId> {
    Some(ProtoStateId {
        value: state_id.as_bytes().to_vec(),
    })
}

pub(super) fn parse_proto_state_id(
    state_id: Option<ProtoStateId>,
) -> Result<Option<StateId>, ProtocolError> {
    state_id
        .map(|state_id| {
            let value: [u8; 32] = state_id.value.try_into().map_err(|value: Vec<u8>| {
                ProtocolError::InvalidState(format!(
                    "state ID must be 32 bytes, got {}",
                    value.len()
                ))
            })?;
            Ok(StateId::from_bytes(value))
        })
        .transpose()
}

pub(super) fn to_protocol_namespace(namespace: HostedNamespace) -> wire::HostedNamespaceInfo {
    use api::heddle::api::v1alpha1::NamespaceKind;
    let kind = match NamespaceKind::try_from(namespace.kind).unwrap_or(NamespaceKind::Unspecified) {
        NamespaceKind::User => "user",
        NamespaceKind::Org => "namespace",
        NamespaceKind::Team => "team",
        NamespaceKind::Unspecified => "",
    };
    wire::HostedNamespaceInfo {
        namespace_id: namespace.namespace_id,
        kind: kind.to_string(),
        slug: namespace.slug,
        parent_id: (!namespace.parent_id.is_empty()).then_some(namespace.parent_id),
        display_name: (!namespace.display_name.is_empty()).then_some(namespace.display_name),
        full_path: namespace.full_path,
    }
}

pub(super) fn to_protocol_repository(repository: HostedRepository) -> wire::HostedRepositoryInfo {
    wire::HostedRepositoryInfo {
        repo_id: repository.repo_id,
        namespace_id: repository.namespace_id,
        slug: repository.slug,
        path: repository.path.into(),
        full_path: repository.full_path,
    }
}

pub(super) fn to_protocol_grant(grant: HostedGrant) -> wire::HostedGrantInfo {
    use api::heddle::api::v1alpha1::grant_target_ref::Target;
    let (namespace_path, repo_path) = match grant.target.and_then(|t| t.target) {
        Some(Target::NamespacePath(p)) if !p.is_empty() => (Some(p), None),
        Some(Target::RepoPath(p)) => (None, repository_ref_path(&p).map(ToOwned::to_owned)),
        _ => (None, None),
    };
    wire::HostedGrantInfo {
        subject: grant.subject,
        role: hosted_role_proto_to_string(grant.role),
        namespace_path,
        repo_path,
    }
}

/// Render a proto `HostedRole` (i32) as the lowercase string the
/// CLI/web tier consumes (`reader` / `developer` / `maintainer` /
/// `admin` / `owner`). Unknown / `UNSPECIFIED` becomes `""`.
pub(super) fn hosted_role_proto_to_string(role: i32) -> String {
    use api::heddle::api::v1alpha1::HostedRole;
    match HostedRole::try_from(role).unwrap_or(HostedRole::Unspecified) {
        HostedRole::Reader => "reader".into(),
        HostedRole::Developer => "developer".into(),
        HostedRole::Maintainer => "maintainer".into(),
        HostedRole::Admin => "admin".into(),
        HostedRole::Owner => "owner".into(),
        HostedRole::Unspecified => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_repository_targets_preserve_both_reference_variants() {
        use api::heddle::api::v1alpha1::{GrantTargetRef, HostedRole, grant_target_ref::Target};

        for reference in [
            Reference::HostedId("repo_123".to_string()),
            Reference::CanonicalPath("acme/widgets".to_string()),
        ] {
            let expected = match &reference {
                Reference::HostedId(value) | Reference::CanonicalPath(value) => value.clone(),
            };
            let grant = HostedGrant {
                subject: "principal:alice".to_string(),
                role: HostedRole::Developer as i32,
                target: Some(GrantTargetRef {
                    target: Some(Target::RepoPath(RepositoryRef {
                        reference: Some(reference),
                    })),
                }),
            };

            let mapped = to_protocol_grant(grant);

            assert_eq!(mapped.repo_path.as_deref(), Some(expected.as_str()));
            assert_eq!(mapped.namespace_path, None);
        }
    }

    #[test]
    fn native_auth_failure_maps_without_transport_status_types() {
        let error = hosted_to_protocol_error(HostedError::Call {
            code: api::heddle::api::v1alpha1::CallFailureCode::Unauthenticated,
            message: "invalid proof".to_string(),
            details: Vec::new(),
        });
        assert!(matches!(error, ProtocolError::AuthorizationFailed(_)));
    }

    #[test]
    fn native_call_failure_preserves_typed_and_unknown_details() {
        use api::heddle::api::v1alpha1::ConflictDetail;
        use prost::Message as _;

        let conflict = ConflictDetail {
            resource: "refs/heads/main".to_string(),
            expected_version: "old".to_string(),
            actual_version: "new".to_string(),
        };
        let error = hosted_to_protocol_error(HostedError::Call {
            code: api::heddle::api::v1alpha1::CallFailureCode::AlreadyExists,
            message: "ref changed".to_string(),
            details: vec![
                prost_types::Any {
                    type_url: "type.googleapis.com/heddle.api.v1alpha1.ConflictDetail".to_string(),
                    value: conflict.encode_to_vec(),
                },
                prost_types::Any {
                    type_url: "type.example.test/FutureDetail".to_string(),
                    value: vec![1, 2, 3],
                },
            ],
        });

        let ProtocolError::RemoteFailure { code, details, .. } = error else {
            panic!("expected remote failure");
        };
        assert_eq!(code, wire::RemoteFailureCode::AlreadyExists);
        assert!(matches!(
            &details[0],
            wire::RemoteFailureDetail::Conflict { resource, .. }
                if resource == "refs/heads/main"
        ));
        assert!(matches!(
            &details[1],
            wire::RemoteFailureDetail::Unknown { type_url, value }
                if type_url == "type.example.test/FutureDetail" && value == &[1, 2, 3]
        ));
    }
}
