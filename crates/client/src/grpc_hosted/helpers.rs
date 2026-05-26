use core::convert::TryFrom;

use base64::Engine as _;
use cli_shared::ClientConfig;
use grpc::heddle::v1::{
    HostedGrant, HostedNamespace, HostedRepository, ObjectAvailabilityStatus, ObjectDescriptor,
    TransferCheckpoint, TransportMode,
};
use objects::object::{ChangeId, ContentHash};
use proto::{
    CAPABILITY_CHUNKED_TRANSFER, CAPABILITY_PACK_TRANSFER, CAPABILITY_PARTIAL_FETCH,
    CAPABILITY_RESUMABLE_TRANSFER, Capabilities, CapabilitySet, ObjectId, ObjectInfo, ObjectType,
    ProtocolError,
};
use tonic::Status;

#[derive(Debug, Clone)]
pub(crate) struct HostedTransportPolicy {
    pub chunk_size: usize,
    pub max_inflight_objects: usize,
    pub resume_attempts: usize,
    pub negotiated: CapabilitySet,
}

impl HostedTransportPolicy {
    pub fn from_client_config(config: &ClientConfig) -> Self {
        let mut client_caps = Capabilities::default()
            .with_chunk_size(config.chunk_size.min(u32::MAX as usize) as u32);
        if config.chunked_transfer {
            client_caps = client_caps.with_flag(CAPABILITY_CHUNKED_TRANSFER);
        }
        if config.resumable_transfer {
            client_caps = client_caps.with_flag(CAPABILITY_RESUMABLE_TRANSFER);
        }
        if config.pack_transfer {
            client_caps = client_caps.with_flag(CAPABILITY_PACK_TRANSFER);
        }
        if config.partial_fetch {
            client_caps = client_caps.with_flag(CAPABILITY_PARTIAL_FETCH);
        }

        let server_caps = Capabilities::default()
            .with_flag(CAPABILITY_CHUNKED_TRANSFER)
            .with_flag(CAPABILITY_RESUMABLE_TRANSFER)
            .with_flag(CAPABILITY_PACK_TRANSFER)
            .with_flag(CAPABILITY_PARTIAL_FETCH)
            .with_chunk_size(config.chunk_size.min(u32::MAX as usize) as u32);
        let negotiated = CapabilitySet::new(&client_caps, &server_caps);
        let chunk_size = negotiated.chunk_size().max(1);
        let max_inflight_objects = (chunk_size / (16 * 1024)).clamp(1, 16);
        Self {
            chunk_size,
            max_inflight_objects,
            resume_attempts: 2,
            negotiated,
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
        ObjectType::State => Ok(ObjectId::ChangeId(
            ChangeId::parse(value).map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
        )),
        ObjectType::Blob | ObjectType::Tree | ObjectType::Action | ObjectType::Redaction => {
            Ok(ObjectId::Hash(ContentHash::from_hex(value).map_err(
                |err| ProtocolError::InvalidState(err.to_string()),
            )?))
        }
    }
}

pub(super) fn parse_object_type(value: &str) -> Result<ObjectType, ProtocolError> {
    match value {
        "blob" => Ok(ObjectType::Blob),
        "tree" => Ok(ObjectType::Tree),
        "state" => Ok(ObjectType::State),
        "action" => Ok(ObjectType::Action),
        "redaction" => Ok(ObjectType::Redaction),
        _ => Err(ProtocolError::InvalidState(format!(
            "unknown object type: {value}"
        ))),
    }
}

pub(super) fn to_proto_object_info(info: &ObjectInfo) -> ObjectDescriptor {
    ObjectDescriptor {
        id: match &info.id {
            ObjectId::Hash(hash) => hash.to_hex(),
            ObjectId::ChangeId(change_id) => change_id.to_string_full(),
        },
        object_type: object_type_name(info.obj_type).to_string(),
        availability_status: ObjectAvailabilityStatus::Present as i32,
        availability_note: String::new(),
    }
}

pub(super) fn object_descriptor_with_status(
    info: &ObjectInfo,
    availability_status: ObjectAvailabilityStatus,
    availability_note: impl Into<String>,
) -> ObjectDescriptor {
    ObjectDescriptor {
        id: match &info.id {
            ObjectId::Hash(hash) => hash.to_hex(),
            ObjectId::ChangeId(change_id) => change_id.to_string_full(),
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
    match obj_type {
        ObjectType::Blob => "blob",
        ObjectType::Tree => "tree",
        ObjectType::State => "state",
        ObjectType::Action => "action",
        ObjectType::Redaction => "redaction",
    }
}

pub(super) fn descriptor_id(descriptor: &ObjectDescriptor) -> (String, String) {
    (descriptor.id.clone(), descriptor.object_type.clone())
}

pub(super) fn status_to_protocol_error(status: Status) -> ProtocolError {
    match status.code() {
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
            ProtocolError::AuthorizationFailed(status.message().to_string())
        }
        tonic::Code::NotFound => ProtocolError::ObjectNotFound(status.message().to_string()),
        tonic::Code::InvalidArgument => ProtocolError::InvalidState(status.message().to_string()),
        _ => ProtocolError::Remote(status.message().to_string()),
    }
}

pub(super) fn to_protocol_namespace(namespace: HostedNamespace) -> proto::HostedNamespaceInfo {
    use grpc::heddle::v1::NamespaceKind;
    let kind = match NamespaceKind::try_from(namespace.kind).unwrap_or(NamespaceKind::Unspecified) {
        NamespaceKind::User => "user",
        NamespaceKind::Org => "namespace",
        NamespaceKind::Team => "team",
        NamespaceKind::Unspecified => "",
    };
    proto::HostedNamespaceInfo {
        namespace_id: namespace.namespace_id,
        kind: kind.to_string(),
        slug: namespace.slug,
        parent_id: (!namespace.parent_id.is_empty()).then_some(namespace.parent_id),
        display_name: (!namespace.display_name.is_empty()).then_some(namespace.display_name),
        full_path: namespace.full_path,
    }
}

pub(super) fn to_protocol_repository(repository: HostedRepository) -> proto::HostedRepositoryInfo {
    proto::HostedRepositoryInfo {
        repo_id: repository.repo_id,
        namespace_id: repository.namespace_id,
        slug: repository.slug,
        path: repository.path.into(),
        full_path: repository.full_path,
    }
}

pub(super) fn to_protocol_grant(grant: HostedGrant) -> proto::HostedGrantInfo {
    use grpc::heddle::v1::grant_target_ref::Target;
    let (namespace_path, repo_path) = match grant.target.and_then(|t| t.target) {
        Some(Target::NamespacePath(p)) if !p.is_empty() => (Some(p), None),
        Some(Target::RepoPath(p)) if !p.is_empty() => (None, Some(p)),
        _ => (None, None),
    };
    proto::HostedGrantInfo {
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
    use grpc::heddle::v1::HostedRole;
    match HostedRole::try_from(role).unwrap_or(HostedRole::Unspecified) {
        HostedRole::Reader => "reader".into(),
        HostedRole::Developer => "developer".into(),
        HostedRole::Maintainer => "maintainer".into(),
        HostedRole::Admin => "admin".into(),
        HostedRole::Owner => "owner".into(),
        HostedRole::Unspecified => String::new(),
    }
}
