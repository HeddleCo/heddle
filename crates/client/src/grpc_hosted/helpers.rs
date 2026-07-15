use core::convert::TryFrom;

use base64::Engine as _;
use cli_shared::ClientConfig;
use grpc::heddle::api::v1alpha1::{
    HostedGrant, HostedNamespace, HostedRepository, ObjectAvailabilityStatus, ObjectDescriptor,
    RepositoryRef, StateId as ProtoStateId, TransferCheckpoint, TransportMode,
    repository_ref::Reference,
};
use objects::object::{ContentHash, StateAttachmentId, StateId};
use tonic::Status;
use wire::{ObjectId, ObjectInfo, ObjectType, ProtocolError};

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

pub(super) fn parse_descriptor_to_info(
    descriptor: ObjectDescriptor,
) -> Result<ObjectInfo, ProtocolError> {
    let obj_type = parse_object_type(&descriptor.object_type)?;
    let id = parse_object_id(&descriptor.id, obj_type)?;
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
            Ok(ObjectId::StateAttachment {
                state: StateId::parse(state)
                    .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                id: StateAttachmentId::from_hash(
                    ContentHash::from_hex(attachment)
                        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                ),
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
    ObjectDescriptor {
        id: match &info.id {
            ObjectId::Hash(hash) => hash.to_hex(),
            ObjectId::StateId(state_id) => state_id.to_string_full(),
            ObjectId::StateAttachment { state, id } => {
                format!("{}:{}", state.to_string_full(), id.as_hash().to_hex())
            }
        },
        object_type: object_type_name(info.obj_type).to_string(),
        availability_status: availability_status as i32,
        availability_note: availability_note.into(),
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
        ObjectId::StateAttachment { state, id } => {
            format!("{}:{}", state.to_string_full(), id.as_hash().to_hex())
        }
    };
    (id, object_type_name(info.obj_type).to_string())
}

pub(super) fn status_to_protocol_error(status: Status) -> ProtocolError {
    let message = if status.message().contains("missing x-heddle-proof-ts") {
        format!(
            "{}; credential missing device proof key — re-login / re-install the credential with its matching private key",
            status.message()
        )
    } else {
        status.message().to_string()
    };
    match status.code() {
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
            ProtocolError::AuthorizationFailed(message)
        }
        tonic::Code::NotFound => ProtocolError::ObjectNotFound(message),
        tonic::Code::AlreadyExists => ProtocolError::AlreadyExists(message),
        tonic::Code::InvalidArgument => ProtocolError::InvalidState(message),
        _ => ProtocolError::Remote(message),
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

pub(super) fn to_protocol_namespace(namespace: HostedNamespace) -> wire::HostedNamespaceInfo {
    use grpc::heddle::api::v1alpha1::NamespaceKind;
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
    use grpc::heddle::api::v1alpha1::grant_target_ref::Target;
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
    use grpc::heddle::api::v1alpha1::HostedRole;
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
        use grpc::heddle::api::v1alpha1::{GrantTargetRef, HostedRole, grant_target_ref::Target};

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
    fn missing_proof_timestamp_error_explains_how_to_recover() {
        let error = status_to_protocol_error(Status::unauthenticated(
            "missing x-heddle-proof-ts metadata",
        ));
        let message = error.to_string();

        assert!(message.contains("credential missing device proof key"));
        assert!(message.contains("re-login / re-install"));
    }
}
