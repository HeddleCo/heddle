use api::heddle::api::v1alpha1::{
    CallFailureCode, HostedGrant, HostedNamespace, HostedRepository, HostedRole, NamespaceKind,
    RepositoryRef, StateId as ProtoStateId, grant_target_ref::Target, repository_ref::Reference,
};
use objects::object::StateId;
use wire::ProtocolError;

use super::HostedError;

pub(super) fn hosted_to_protocol_error(error: HostedError) -> ProtocolError {
    match error {
        HostedError::Call { code, message, .. } => match code {
            CallFailureCode::Unauthenticated | CallFailureCode::PermissionDenied => {
                ProtocolError::AuthorizationFailed(message)
            }
            CallFailureCode::NotFound => ProtocolError::ObjectNotFound(message),
            CallFailureCode::AlreadyExists => ProtocolError::AlreadyExists(message),
            CallFailureCode::InvalidArgument | CallFailureCode::FailedPrecondition => {
                ProtocolError::InvalidState(message)
            }
            _ => ProtocolError::Remote(message),
        },
        HostedError::Decode(error) => ProtocolError::Serialization(error.to_string()),
        HostedError::Transport(message) => ProtocolError::Io(std::io::Error::other(message)),
        error => ProtocolError::Remote(error.to_string()),
    }
}

pub(super) fn repository_ref(path: &str) -> Option<RepositoryRef> {
    Some(RepositoryRef {
        reference: Some(Reference::CanonicalPath(path.to_string())),
    })
}

pub(super) fn proto_state_id(state_id: StateId) -> Option<ProtoStateId> {
    Some(ProtoStateId {
        value: state_id.as_bytes().to_vec(),
    })
}

pub(super) fn repository_ref_path(repository: &RepositoryRef) -> Option<&str> {
    match repository.reference.as_ref() {
        Some(Reference::HostedId(id) | Reference::CanonicalPath(id)) if !id.is_empty() => Some(id),
        _ => None,
    }
}

pub(super) fn to_protocol_namespace(namespace: HostedNamespace) -> wire::HostedNamespaceInfo {
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
    let (namespace_path, repo_path) = match grant.target.and_then(|target| target.target) {
        Some(Target::NamespacePath(path)) if !path.is_empty() => (Some(path), None),
        Some(Target::RepoPath(path)) => (None, repository_ref_path(&path).map(ToOwned::to_owned)),
        _ => (None, None),
    };
    wire::HostedGrantInfo {
        subject: grant.subject,
        role: match HostedRole::try_from(grant.role).unwrap_or(HostedRole::Unspecified) {
            HostedRole::Reader => "reader",
            HostedRole::Developer => "developer",
            HostedRole::Maintainer => "maintainer",
            HostedRole::Admin => "admin",
            HostedRole::Owner => "owner",
            HostedRole::Unspecified => "",
        }
        .to_string(),
        namespace_path,
        repo_path,
    }
}
