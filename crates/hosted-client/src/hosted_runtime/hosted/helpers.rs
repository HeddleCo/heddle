use core::convert::TryFrom;
use std::time::Duration;

use api::heddle::api::v1alpha1::{
    HostedGrant, HostedObjectType, HostedSpool, ObjectAvailabilityStatus, ObjectDescriptor,
    RepositoryRef, StateAttachmentKind as ProtoStateAttachmentKind, StateId as ProtoStateId,
    TransferCheckpoint, TransportMode, repository_ref::Reference,
};
use base64::Engine as _;
use config::ClientConfig;
use objects::object::{ContentHash, StateAttachmentId, StateAttachmentKind, StateId};
use wire::{ObjectId, ObjectInfo, ObjectType, ProtocolError};

use super::HostedError;

#[derive(Debug, Clone)]
pub(crate) struct HostedTransportPolicy {
    pub chunk_size: usize,
    pub max_inflight_objects: usize,
    pub resume_attempts: usize,
    pub provider_global_concurrency: usize,
    pub provider_per_endpoint_concurrency: usize,
    pub provider_max_inflight_bytes: usize,
    pub provider_stall_timeout: Duration,
}

impl HostedTransportPolicy {
    pub fn from_client_config(config: &ClientConfig) -> Self {
        let chunk_size = config.chunk_size.max(1);
        let max_inflight_objects = (chunk_size / (16 * 1024)).clamp(1, 16);
        let provider_global_concurrency = config.provider_global_concurrency.clamp(1, 64);
        Self {
            chunk_size,
            max_inflight_objects,
            resume_attempts: 2,
            provider_global_concurrency,
            provider_per_endpoint_concurrency: config
                .provider_per_endpoint_concurrency
                .clamp(1, provider_global_concurrency),
            provider_max_inflight_bytes: config.provider_max_inflight_bytes.max(1),
            provider_stall_timeout: Duration::from_secs(config.provider_stall_timeout_secs.max(1)),
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
    let obj_type = parse_object_type(descriptor.object_type)?;
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
        ObjectType::Blob
        | ObjectType::Tree
        | ObjectType::Action
        | ObjectType::AnnotatedTag
        | ObjectType::Redaction
        | ObjectType::Purge
        | ObjectType::KeyBinding => Ok(ObjectId::Hash(
            ContentHash::from_hex(value)
                .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
        )),
    }
}

pub(super) fn parse_object_type(value: i32) -> Result<ObjectType, ProtocolError> {
    // heddle-api 0.10 has no key-binding or annotated-tag closure variants.
    // Prost preserves unknown discriminants, so recognize Heddle's extension
    // values before asking the generated enum to decode them.
    if value == HOSTED_OBJECT_TYPE_KEY_BINDING {
        return Ok(ObjectType::KeyBinding);
    }
    if value == HOSTED_OBJECT_TYPE_ANNOTATED_TAG {
        return Ok(ObjectType::AnnotatedTag);
    }
    match HostedObjectType::try_from(value).unwrap_or_default() {
        HostedObjectType::Blob => Ok(ObjectType::Blob),
        HostedObjectType::Tree => Ok(ObjectType::Tree),
        HostedObjectType::State => Ok(ObjectType::State),
        HostedObjectType::Action => Ok(ObjectType::Action),
        HostedObjectType::Redaction => Ok(ObjectType::Redaction),
        HostedObjectType::StateVisibility => Ok(ObjectType::StateVisibility),
        HostedObjectType::StateAttachment => Ok(ObjectType::StateAttachment),
        HostedObjectType::Purge => Ok(ObjectType::Purge),
        HostedObjectType::Unspecified => Err(ProtocolError::InvalidState(
            "object descriptor is missing object_type".to_string(),
        )),
    }
}

const HOSTED_OBJECT_TYPE_KEY_BINDING: i32 = 10;
const HOSTED_OBJECT_TYPE_ANNOTATED_TAG: i32 = 9;

fn object_type_to_proto(obj_type: ObjectType) -> i32 {
    match obj_type {
        ObjectType::Blob => HostedObjectType::Blob as i32,
        ObjectType::Tree => HostedObjectType::Tree as i32,
        ObjectType::State => HostedObjectType::State as i32,
        ObjectType::Action => HostedObjectType::Action as i32,
        ObjectType::AnnotatedTag => HOSTED_OBJECT_TYPE_ANNOTATED_TAG,
        ObjectType::Redaction => HostedObjectType::Redaction as i32,
        ObjectType::Purge => HostedObjectType::Purge as i32,
        ObjectType::StateVisibility => HostedObjectType::StateVisibility as i32,
        ObjectType::StateAttachment => HostedObjectType::StateAttachment as i32,
        ObjectType::KeyBinding => HOSTED_OBJECT_TYPE_KEY_BINDING,
    }
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
        object_type: object_type_to_proto(info.obj_type),
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

pub(super) fn descriptor_id(descriptor: &ObjectDescriptor) -> (String, i32) {
    (descriptor.id.clone(), descriptor.object_type)
}

/// Compute the same `(id, object_type)` key as
/// `descriptor_id(&to_proto_object_info(info))` without the throwaway full
/// proto encode. Must stay byte-identical to the descriptor the server keys on.
pub(super) fn descriptor_id_from_info(info: &ObjectInfo) -> (String, i32) {
    let id = match &info.id {
        ObjectId::Hash(hash) => hash.to_hex(),
        ObjectId::StateId(state_id) => state_id.to_string_full(),
        ObjectId::StateAttachment { state, id, kind: _ } => {
            format!("{}:{}", state.to_string_full(), id.as_hash().to_hex())
        }
    };
    (id, object_type_to_proto(info.obj_type))
}

pub(super) fn hosted_to_protocol_error(error: HostedError) -> ProtocolError {
    use api::heddle::api::v1alpha1::CallFailureCode;
    match error {
        HostedError::Call {
            code,
            message,
            error,
        } => {
            if let Some(error) = error {
                return ProtocolError::RemoteFailure {
                    code: remote_failure_code(code),
                    message,
                    details: vec![remote_failure_detail(*error)],
                };
            }

            match code {
                CallFailureCode::PermissionDenied => ProtocolError::AuthorizationFailed(message),
                // Keep the CallFailureCode so doorbell fetch can treat
                // expired/missing creds as fatal instead of a visibility skip.
                CallFailureCode::Unauthenticated => ProtocolError::RemoteFailure {
                    code: remote_failure_code(code),
                    message,
                    details: Vec::new(),
                },
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

fn remote_failure_detail(
    detail: api::heddle::api::v1alpha1::ErrorDetail,
) -> wire::RemoteFailureDetail {
    use api::heddle::api::v1alpha1::error_detail::Context;
    use prost::Message as _;

    let encoded = detail.encode_to_vec();
    match detail.context {
        Some(Context::Retry(value)) => wire::RemoteFailureDetail::Retry {
            retry_after: value.retry_after.map(remote_duration),
        },
        Some(Context::Conflict(value)) => wire::RemoteFailureDetail::Conflict {
            resource: value.resource,
            expected_version: value.expected_version,
            actual_version: value.actual_version,
        },
        Some(Context::Cursor(value)) => wire::RemoteFailureDetail::Cursor(remote_cursor(value)),
        Some(Context::Capability(value)) => wire::RemoteFailureDetail::CapabilityRequirement {
            capabilities: value.capabilities,
        },
        Some(Context::Policy(value)) => wire::RemoteFailureDetail::PolicyDenial {
            policy_id: value.policy_id,
            rule: value.rule,
            human_verification_can_override: value.human_verification_can_override,
        },
        Some(Context::Stream(value)) => remote_stream_failure(*value),
        Some(Context::Unknown(value)) => wire::RemoteFailureDetail::Unknown {
            type_url: value.type_url,
            value: value.value,
        },
        Some(Context::HumanVerification(_))
        | Some(Context::AmbiguousChangeId(_))
        | Some(Context::Signup(_))
        | None => wire::RemoteFailureDetail::Unknown {
            type_url: "type.googleapis.com/heddle.api.v1alpha1.ErrorDetail".to_string(),
            value: encoded,
        },
    }
}

fn remote_stream_failure(
    value: api::heddle::api::v1alpha1::StreamFailure,
) -> wire::RemoteFailureDetail {
    use api::heddle::api::v1alpha1::{CallFailureCode, error_detail::Context};

    let (retry_after, cursor) = match value.error.and_then(|detail| detail.context) {
        Some(Context::Retry(retry)) => (retry.retry_after.map(remote_duration), None),
        Some(Context::Cursor(cursor)) => (None, Some(remote_cursor(cursor))),
        _ => (None, None),
    };
    wire::RemoteFailureDetail::Stream {
        code: remote_failure_code(CallFailureCode::try_from(value.code).unwrap_or_default()),
        message: value.message,
        retry_after,
        cursor,
    }
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

pub(super) fn to_protocol_spool(spool: HostedSpool) -> wire::HostedSpoolInfo {
    wire::HostedSpoolInfo {
        spool_id: spool.spool_id,
        full_path: spool.full_path,
        kind: spool.kind,
        is_repo: spool.is_repo,
        display_name: (!spool.display_name.is_empty()).then_some(spool.display_name),
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
    use api::heddle::api::v1alpha1::{CallFailure, CallFailureCode, StreamFailure};

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
            error: None,
        });
        assert!(matches!(
            error,
            ProtocolError::RemoteFailure {
                code: wire::RemoteFailureCode::Unauthenticated,
                ..
            }
        ));
    }

    #[test]
    fn key_binding_descriptor_roundtrips_through_the_forward_proto_discriminant() {
        let hash = ContentHash::from_bytes([0x61; 32]);
        let info = ObjectInfo {
            id: ObjectId::Hash(hash),
            obj_type: ObjectType::KeyBinding,
            size: 0,
            delta_base: None,
        };

        let descriptor = to_proto_object_info(&info);
        assert_eq!(descriptor.object_type, HOSTED_OBJECT_TYPE_KEY_BINDING);
        let parsed = parse_descriptor_to_info(descriptor).expect("parse key-binding descriptor");

        assert_eq!(parsed.id, ObjectId::Hash(hash));
        assert_eq!(parsed.obj_type, ObjectType::KeyBinding);
    }

    #[test]
    fn native_call_failure_preserves_typed_error_detail() {
        use api::heddle::api::v1alpha1::{ConflictDetail, ErrorDetail, ErrorReason, error_detail};

        let error = hosted_to_protocol_error(HostedError::Call {
            code: api::heddle::api::v1alpha1::CallFailureCode::AlreadyExists,
            message: "ref changed".to_string(),
            error: Some(Box::new(ErrorDetail {
                reason: ErrorReason::VersionConflict as i32,
                resource: "refs/heads/main".to_string(),
                field: String::new(),
                context: Some(error_detail::Context::Conflict(ConflictDetail {
                    resource: "refs/heads/main".to_string(),
                    expected_version: "old".to_string(),
                    actual_version: "new".to_string(),
                })),
            })),
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
    }

    #[test]
    fn attachment_kind_roundtrips_through_proto() {
        use objects::object::StateAttachmentKind;
        for kind in [
            StateAttachmentKind::Context,
            StateAttachmentKind::RiskSignals,
            StateAttachmentKind::ReviewSignatures,
            StateAttachmentKind::Discussions,
            StateAttachmentKind::StructuredConflicts,
            StateAttachmentKind::SemanticIndex,
            StateAttachmentKind::Signature,
        ] {
            let proto = attachment_kind_to_proto(kind);
            assert_eq!(attachment_kind_from_proto(proto), Some(kind));
        }
        assert_eq!(
            attachment_kind_from_proto(ProtoStateAttachmentKind::Unspecified),
            None
        );
    }

    #[test]
    fn parse_object_type_and_id_cover_every_object_kind() {
        use objects::object::{ContentHash, StateAttachmentKind, StateId};

        assert_eq!(
            parse_object_type(HostedObjectType::Blob as i32).unwrap(),
            ObjectType::Blob
        );
        assert_eq!(
            parse_object_type(HostedObjectType::Tree as i32).unwrap(),
            ObjectType::Tree
        );
        assert_eq!(
            parse_object_type(HostedObjectType::State as i32).unwrap(),
            ObjectType::State
        );
        assert_eq!(
            parse_object_type(HostedObjectType::Action as i32).unwrap(),
            ObjectType::Action
        );
        assert_eq!(
            parse_object_type(HostedObjectType::Redaction as i32).unwrap(),
            ObjectType::Redaction
        );
        assert_eq!(
            parse_object_type(HostedObjectType::StateVisibility as i32).unwrap(),
            ObjectType::StateVisibility
        );
        assert_eq!(
            parse_object_type(HostedObjectType::StateAttachment as i32).unwrap(),
            ObjectType::StateAttachment
        );
        assert_eq!(
            parse_object_type(HostedObjectType::Purge as i32).unwrap(),
            ObjectType::Purge
        );
        assert_eq!(
            parse_object_type(HOSTED_OBJECT_TYPE_KEY_BINDING).unwrap(),
            ObjectType::KeyBinding
        );
        assert!(parse_object_type(HostedObjectType::Unspecified as i32).is_err());

        let hash = ContentHash::from_bytes([0x11; 32]);
        let hash_hex = hash.to_hex();
        let state = StateId::from_bytes([0x22; 32]);
        let state_full = state.to_string_full();

        let blob = parse_object_id(&hash_hex, ObjectType::Blob, None).unwrap();
        assert!(matches!(blob, ObjectId::Hash(h) if h == hash));

        let state_id = parse_object_id(&state_full, ObjectType::State, None).unwrap();
        assert!(matches!(state_id, ObjectId::StateId(s) if s == state));

        let visibility = parse_object_id(&state_full, ObjectType::StateVisibility, None).unwrap();
        assert!(matches!(visibility, ObjectId::StateId(s) if s == state));

        let attachment_locator = format!("{state_full}:{hash_hex}");
        let attachment = parse_object_id(
            &attachment_locator,
            ObjectType::StateAttachment,
            Some(StateAttachmentKind::Context),
        )
        .unwrap();
        assert!(matches!(
            attachment,
            ObjectId::StateAttachment {
                kind: StateAttachmentKind::Context,
                ..
            }
        ));
        assert!(
            parse_object_id(&attachment_locator, ObjectType::StateAttachment, None).is_err(),
            "attachment without kind must fail"
        );
        assert!(
            parse_object_id(
                "no-colon",
                ObjectType::StateAttachment,
                Some(StateAttachmentKind::Discussions)
            )
            .is_err()
        );
    }

    #[test]
    fn object_descriptor_roundtrips_with_status_and_attachment_kind() {
        use objects::object::{ContentHash, StateAttachmentId, StateAttachmentKind, StateId};
        use wire::ObjectInfo;

        let hash = ContentHash::from_bytes([0x33; 32]);
        let info = ObjectInfo {
            id: ObjectId::Hash(hash),
            obj_type: ObjectType::Blob,
            size: 0,
            delta_base: None,
        };
        let descriptor =
            object_descriptor_with_status(&info, ObjectAvailabilityStatus::Missing, "not local");
        assert_eq!(descriptor.id, hash.to_hex());
        assert_eq!(
            descriptor.availability_status,
            ObjectAvailabilityStatus::Missing as i32
        );
        assert_eq!(descriptor.availability_note, "not local");
        let parsed = parse_descriptor_to_info(to_proto_object_info(&info)).unwrap();
        assert_eq!(parsed.id, ObjectId::Hash(hash));

        let state = StateId::from_bytes([0x44; 32]);
        let attachment_info = ObjectInfo {
            id: ObjectId::StateAttachment {
                state,
                id: StateAttachmentId::from_hash(hash),
                kind: StateAttachmentKind::Discussions,
            },
            obj_type: ObjectType::StateAttachment,
            size: 0,
            delta_base: None,
        };
        let descriptor = to_proto_object_info(&attachment_info);
        let parsed = parse_descriptor_to_info(descriptor).unwrap();
        assert!(matches!(
            parsed.id,
            ObjectId::StateAttachment {
                kind: StateAttachmentKind::Discussions,
                ..
            }
        ));

        // Attachment descriptor with unspecified kind is a hard error.
        let mut bad = to_proto_object_info(&attachment_info);
        bad.attachment_kind = ProtoStateAttachmentKind::Unspecified as i32;
        assert!(parse_descriptor_to_info(bad).is_err());
    }

    #[test]
    fn decode_blob_content_handles_text_and_base64() {
        assert_eq!(
            decode_blob_content("hello".into(), false).unwrap(),
            b"hello"
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"\0\x01\x02");
        assert_eq!(decode_blob_content(encoded, true).unwrap(), vec![0, 1, 2]);
        assert!(decode_blob_content("!!!".into(), true).is_err());
    }

    #[test]
    fn hosted_to_protocol_error_maps_call_codes_without_detail() {
        use api::heddle::api::v1alpha1::CallFailureCode;

        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::Unauthenticated,
                message: "nope".into(),
                error: None,
            }),
            ProtocolError::RemoteFailure {
                code: wire::RemoteFailureCode::Unauthenticated,
                ..
            }
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::PermissionDenied,
                message: "nope".into(),
                error: None,
            }),
            ProtocolError::AuthorizationFailed(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::NotFound,
                message: "missing".into(),
                error: None,
            }),
            ProtocolError::ObjectNotFound(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::AlreadyExists,
                message: "exists".into(),
                error: None,
            }),
            ProtocolError::AlreadyExists(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::InvalidArgument,
                message: "bad".into(),
                error: None,
            }),
            ProtocolError::InvalidState(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::FailedPrecondition,
                message: "pre".into(),
                error: None,
            }),
            ProtocolError::InvalidState(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::Internal,
                message: "boom".into(),
                error: None,
            }),
            ProtocolError::RemoteFailure { .. }
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Transport("down".into())),
            ProtocolError::Io(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Framing("bad frame".into())),
            ProtocolError::Remote(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::SigningIdentityRequired),
            ProtocolError::Remote(_)
        ));
    }

    #[test]
    fn remote_failure_detail_maps_every_context_variant() {
        use api::heddle::api::v1alpha1::{
            CapabilityRequirement, ConflictDetail, CursorFailure, ErrorDetail, ErrorReason,
            PolicyDenial, RetryAdvice, error_detail,
        };

        let retry = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Retry(RetryAdvice {
                retry_after: Some(prost_types::Duration {
                    seconds: 1,
                    nanos: 0,
                }),
            })),
        });
        assert!(matches!(retry, wire::RemoteFailureDetail::Retry { .. }));

        let conflict = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::VersionConflict as i32,
            resource: "r".into(),
            field: String::new(),
            context: Some(error_detail::Context::Conflict(ConflictDetail {
                resource: "r".into(),
                expected_version: "a".into(),
                actual_version: "b".into(),
            })),
        });
        assert!(matches!(
            conflict,
            wire::RemoteFailureDetail::Conflict { .. }
        ));

        let cursor = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Cursor(CursorFailure {
                reason: 1,
                expired_at: None,
                restart_cursor: "cursor-token".into(),
            })),
        });
        assert!(matches!(cursor, wire::RemoteFailureDetail::Cursor(_)));

        let capability = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Capability(CapabilityRequirement {
                capabilities: vec!["push".into()],
            })),
        });
        assert!(matches!(
            capability,
            wire::RemoteFailureDetail::CapabilityRequirement { .. }
        ));

        let policy = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Policy(PolicyDenial {
                policy_id: "p1".into(),
                rule: "r1".into(),
                human_verification_can_override: true,
            })),
        });
        assert!(matches!(
            policy,
            wire::RemoteFailureDetail::PolicyDenial { .. }
        ));

        let unknown = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: None,
        });
        assert!(matches!(unknown, wire::RemoteFailureDetail::Unknown { .. }));
    }

    #[test]
    fn protocol_spool_grant_and_role_mappers() {
        use api::heddle::api::v1alpha1::{
            GrantTargetRef, HostedGrant, HostedRole, HostedSpool, grant_target_ref::Target,
        };

        let spool = to_protocol_spool(HostedSpool {
            spool_id: "s1".into(),
            full_path: "acme/spool".into(),
            kind: "repo".into(),
            is_repo: true,
            display_name: "Spool".into(),
            ..Default::default()
        });
        assert_eq!(spool.full_path, "acme/spool");
        assert_eq!(spool.display_name.as_deref(), Some("Spool"));

        let grant = to_protocol_grant(HostedGrant {
            subject: "principal:alice".into(),
            role: HostedRole::Admin as i32,
            target: Some(GrantTargetRef {
                target: Some(Target::NamespacePath("acme".into())),
            }),
        });
        assert_eq!(grant.namespace_path.as_deref(), Some("acme"));
        assert_eq!(
            hosted_role_proto_to_string(HostedRole::Reader as i32),
            "reader"
        );
        assert_eq!(
            hosted_role_proto_to_string(HostedRole::Developer as i32),
            "developer"
        );
        assert_eq!(
            hosted_role_proto_to_string(HostedRole::Maintainer as i32),
            "maintainer"
        );
        assert_eq!(
            hosted_role_proto_to_string(HostedRole::Admin as i32),
            "admin"
        );
        assert_eq!(
            hosted_role_proto_to_string(HostedRole::Owner as i32),
            "owner"
        );
        assert_eq!(
            hosted_role_proto_to_string(HostedRole::Unspecified as i32),
            ""
        );

        let path = repository_ref("acme/widgets").expect("repo ref");
        assert_eq!(repository_ref_path(&path), Some("acme/widgets"));

        let state = StateId::from_bytes([0x55; 32]);
        let proto = proto_state_id(state).expect("proto state");
        let parsed = parse_proto_state_id(Some(proto))
            .expect("parse")
            .expect("present");
        assert_eq!(parsed, state);
        assert_eq!(parse_proto_state_id(None).expect("none ok"), None);
        let bad = ProtoStateId {
            value: vec![1, 2, 3],
        };
        assert!(parse_proto_state_id(Some(bad)).is_err());
    }

    #[test]
    fn transport_mode_name_covers_known_modes() {
        use api::heddle::api::v1alpha1::TransportMode;
        assert_eq!(
            transport_mode_name(TransportMode::Unspecified as i32),
            "unspecified"
        );
        assert_eq!(
            transport_mode_name(TransportMode::NativePack as i32),
            "native-pack"
        );
        // Any unrecognized value falls back through Unspecified.
        assert_eq!(transport_mode_name(999), "unspecified");
    }

    #[test]
    fn descriptor_id_helpers_key_on_id_and_type() {
        use objects::object::ContentHash;
        use wire::ObjectInfo;

        let hash = ContentHash::from_bytes([0x66; 32]);
        let info = ObjectInfo {
            id: ObjectId::Hash(hash),
            obj_type: ObjectType::Tree,
            size: 0,
            delta_base: None,
        };
        let descriptor = to_proto_object_info(&info);
        assert_eq!(descriptor_id(&descriptor), descriptor_id_from_info(&info));
    }

    /// Push a `CallFailure` through heddle's unary wire path: framing encode,
    /// framing decode, `HostedError`, then the typed detail conversion.
    fn decoded_failure_details(failure: &CallFailure) -> Vec<wire::RemoteFailureDetail> {
        use api::framing::{decode_response_frame, encode_failure_response};

        let encoded = encode_failure_response(failure).expect("encode failure frame");
        let api::framing::ResponseFrame::Failure(decoded) =
            decode_response_frame(&encoded).expect("decode failure frame")
        else {
            panic!("expected a failure frame");
        };
        match hosted_to_protocol_error(decoded.into()) {
            ProtocolError::RemoteFailure { details, .. } => details,
            other => panic!("expected remote failure, got {other:?}"),
        }
    }

    #[test]
    fn every_known_failure_detail_arm_survives_heddles_wire_path() {
        use api::heddle::api::v1alpha1::{
            CapabilityRequirement, ConflictDetail, CursorFailure, ErrorDetail, ErrorReason,
            PolicyDenial, RetryAdvice, UnknownDetail, cursor_failure, error_detail,
        };
        use prost::Message as _;

        let detail = |context| ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(context),
        };
        let failure = |context| CallFailure {
            code: CallFailureCode::FailedPrecondition as i32,
            message: "call failed".to_string(),
            error: Some(detail(context)),
        };

        let retry = failure(error_detail::Context::Retry(RetryAdvice {
            retry_after: Some(prost_types::Duration {
                seconds: 3,
                nanos: 0,
            }),
        }));
        assert_eq!(
            decoded_failure_details(&retry),
            vec![wire::RemoteFailureDetail::Retry {
                retry_after: Some(wire::RemoteDuration {
                    seconds: 3,
                    nanos: 0
                }),
            }]
        );

        let conflict = failure(error_detail::Context::Conflict(ConflictDetail {
            resource: "refs/heads/main".to_string(),
            expected_version: "old".to_string(),
            actual_version: "new".to_string(),
        }));
        assert_eq!(
            decoded_failure_details(&conflict),
            vec![wire::RemoteFailureDetail::Conflict {
                resource: "refs/heads/main".to_string(),
                expected_version: "old".to_string(),
                actual_version: "new".to_string(),
            }]
        );

        let cursor = failure(error_detail::Context::Cursor(CursorFailure {
            reason: cursor_failure::Reason::Stale as i32,
            expired_at: None,
            restart_cursor: "page-42".to_string(),
        }));
        assert_eq!(
            decoded_failure_details(&cursor),
            vec![wire::RemoteFailureDetail::Cursor(
                wire::RemoteCursorFailure {
                    reason: wire::RemoteCursorReason::Stale,
                    expired_at: None,
                    restart_cursor: "page-42".to_string(),
                }
            )]
        );

        let capability = failure(error_detail::Context::Capability(CapabilityRequirement {
            capabilities: vec!["repo.pull".to_string()],
        }));
        assert_eq!(
            decoded_failure_details(&capability),
            vec![wire::RemoteFailureDetail::CapabilityRequirement {
                capabilities: vec!["repo.pull".to_string()],
            }]
        );

        let policy = failure(error_detail::Context::Policy(PolicyDenial {
            policy_id: "retention".to_string(),
            rule: "no-purge".to_string(),
            human_verification_can_override: false,
        }));
        assert_eq!(
            decoded_failure_details(&policy),
            vec![wire::RemoteFailureDetail::PolicyDenial {
                policy_id: "retention".to_string(),
                rule: "no-purge".to_string(),
                human_verification_can_override: false,
            }]
        );

        // An arm from a newer contract version passes through losslessly and
        // its opaque payload still decodes into the original typed message.
        let future_arm = StreamFailure {
            code: CallFailureCode::Internal as i32,
            message: "from the future".to_string(),
            error: None,
        };
        let unknown = failure(error_detail::Context::Unknown(UnknownDetail {
            type_url: "type.googleapis.com/heddle.api.v1alpha1.StreamFailure".to_string(),
            value: future_arm.encode_to_vec(),
        }));
        let details = decoded_failure_details(&unknown);
        let wire::RemoteFailureDetail::Unknown { type_url, value } = &details[0] else {
            panic!("unknown arm must stay unknown, got {:?}", details[0]);
        };
        assert_eq!(details.len(), 1);
        assert_eq!(
            type_url,
            "type.googleapis.com/heddle.api.v1alpha1.StreamFailure"
        );
        assert_eq!(
            &StreamFailure::decode(value.as_slice()).expect("recovered typed payload"),
            &future_arm
        );
    }

    #[test]
    fn stream_failure_round_trips_with_nested_resume_hints() {
        use api::{
            framing::{StreamFrame, decode_stream_frame, encode_stream_failure},
            heddle::api::v1alpha1::{
                CursorFailure, ErrorDetail, ErrorReason, RetryAdvice, cursor_failure, error_detail,
            },
        };

        let hint = |context| ErrorDetail {
            reason: ErrorReason::Transient as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(context),
        };
        let stream_failure = |hint_context| CallFailure {
            code: CallFailureCode::Unavailable as i32,
            message: "pull stream aborted".to_string(),
            error: Some(ErrorDetail {
                reason: ErrorReason::Transient as i32,
                resource: String::new(),
                field: String::new(),
                context: Some(error_detail::Context::Stream(Box::new(StreamFailure {
                    code: CallFailureCode::Internal as i32,
                    message: "pack writer reset".to_string(),
                    error: Some(Box::new(hint(hint_context))),
                }))),
            }),
        };

        let encoded =
            encode_stream_failure(&stream_failure(error_detail::Context::Retry(RetryAdvice {
                retry_after: Some(prost_types::Duration {
                    seconds: 3,
                    nanos: 0,
                }),
            })))
            .expect("encode stream failure");
        let Some((StreamFrame::Failure(decoded), _)) =
            decode_stream_frame(&encoded).expect("decode stream frame")
        else {
            panic!("expected a stream failure frame");
        };
        let ProtocolError::RemoteFailure {
            code,
            message,
            details,
        } = hosted_to_protocol_error(decoded.into())
        else {
            panic!("expected remote failure");
        };
        assert_eq!(code, wire::RemoteFailureCode::Unavailable);
        assert_eq!(message, "pull stream aborted");
        assert_eq!(
            details,
            vec![wire::RemoteFailureDetail::Stream {
                code: wire::RemoteFailureCode::Internal,
                message: "pack writer reset".to_string(),
                retry_after: Some(wire::RemoteDuration {
                    seconds: 3,
                    nanos: 0
                }),
                cursor: None,
            }]
        );

        let encoded = encode_stream_failure(&stream_failure(error_detail::Context::Cursor(
            CursorFailure {
                reason: cursor_failure::Reason::Stale as i32,
                expired_at: None,
                restart_cursor: "page-42".to_string(),
            },
        )))
        .expect("encode stream failure");
        let Some((StreamFrame::Failure(decoded), _)) =
            decode_stream_frame(&encoded).expect("decode stream frame")
        else {
            panic!("expected a stream failure frame");
        };
        let ProtocolError::RemoteFailure { details, .. } = hosted_to_protocol_error(decoded.into())
        else {
            panic!("expected remote failure");
        };
        assert_eq!(
            details,
            vec![wire::RemoteFailureDetail::Stream {
                code: wire::RemoteFailureCode::Internal,
                message: "pack writer reset".to_string(),
                retry_after: None,
                cursor: Some(wire::RemoteCursorFailure {
                    reason: wire::RemoteCursorReason::Stale,
                    expired_at: None,
                    restart_cursor: "page-42".to_string(),
                }),
            }]
        );
    }
}
