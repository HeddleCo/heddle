//! Retry-key ownership for shared API methods that require a client operation ID.
//!
//! A logical client call creates one value before constructing its request.
//! Call sites then clone or propagate that value across retries and stream
//! frames. The descriptor/capability conformance test below makes additions to
//! Heddle's shipped required-ID surface fail until they are classified here.

use wire::ProtocolError;

/// Shared API methods that both require `client_operation_id` and are marked as
/// shipped in Heddle's client capability ledger.
pub(crate) const REQUIRED_CLIENT_OPERATION_ID_METHODS: &[&str] = &[
    "heddle.api.v1alpha1.IdentityService/CreateServiceAccount",
    "heddle.api.v1alpha1.IdentityService/IssueServiceAccountCredential",
    "heddle.api.v1alpha1.RegistryService/CreateGrant",
    "heddle.api.v1alpha1.RegistryService/CreateInvitation",
    "heddle.api.v1alpha1.RegistryService/CreateNamespace",
    "heddle.api.v1alpha1.RegistryService/CreateRepository",
    "heddle.api.v1alpha1.RegistryService/DeleteGrant",
    "heddle.api.v1alpha1.RegistryService/DeleteNamespace",
    "heddle.api.v1alpha1.RegistryService/DeleteRepository",
    "heddle.api.v1alpha1.RegistryService/GrantSupportAccess",
    "heddle.api.v1alpha1.RegistryService/RevokeSupportAccess",
    "heddle.api.v1alpha1.RegistryService/UpdateGrant",
    "heddle.api.v1alpha1.RegistryService/UpdateNamespace",
    "heddle.api.v1alpha1.RegistryService/UpdateRepository",
    "heddle.api.v1alpha1.RepoSyncService/Push",
    "heddle.api.v1alpha1.RepoSyncService/UpdateRef",
    "heddle.api.v1alpha1.RepositoryService/ReviseContext",
    "heddle.api.v1alpha1.RepositoryService/SetContext",
    "heddle.api.v1alpha1.RepositoryService/SupersedeContext",
    "heddle.api.v1alpha1.WorkflowService/ApproveThread",
    "heddle.api.v1alpha1.WorkflowService/RevokeApproval",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClientOperationId(String);

impl ClientOperationId {
    pub(crate) fn fresh(method: &str) -> Self {
        assert_catalogued(method);
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub(crate) fn caller_or_fresh(method: &str, value: impl Into<String>) -> Self {
        assert_catalogued(method);
        let value = value.into();
        if value.is_empty() {
            Self::fresh(method)
        } else {
            Self(value)
        }
    }

    pub(crate) fn for_required_method(
        method: &str,
        value: impl Into<String>,
    ) -> Result<Self, ProtocolError> {
        assert_catalogued(method);
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolError::InvalidState(format!(
                "{method} requires a non-empty client operation ID"
            )));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn to_wire(&self) -> String {
        self.0.clone()
    }
}

fn assert_catalogued(method: &str) {
    assert!(
        REQUIRED_CLIENT_OPERATION_ID_METHODS.contains(&method),
        "required client operation ID method is missing from the catalog: {method}"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use prost_reflect::{DescriptorPool, Value};

    use super::*;

    #[test]
    fn shipped_required_operation_id_catalog_matches_shared_descriptor() {
        let pool = DescriptorPool::decode(grpc::FILE_DESCRIPTOR_SET)
            .expect("the shared API descriptor must decode");
        let rpc_contract = pool
            .get_extension_by_name("heddle.api.v1alpha1.rpc_contract")
            .expect("the shared descriptor must define rpc_contract");
        let capabilities: serde_json::Value =
            serde_json::from_str(include_str!("../../../../api-capabilities/heddle.json"))
                .expect("Heddle's capability ledger must decode");
        let shipped = capabilities["rpc_mappings"]
            .as_array()
            .expect("rpc_mappings must be an array")
            .iter()
            .filter(|mapping| mapping["layers"]["client"]["status"] == "shipped")
            .map(|mapping| {
                mapping["rpc"]
                    .as_str()
                    .expect("rpc mapping must name an RPC")
            })
            .collect::<BTreeSet<_>>();

        let descriptor_required = pool
            .services()
            .flat_map(|service| service.methods().collect::<Vec<_>>())
            .filter_map(|method| {
                let options = method.options();
                let contract_option = options.get_extension(&rpc_contract);
                let Value::Message(contract) = contract_option.as_ref() else {
                    return None;
                };
                let required = contract
                    .get_field_by_name("client_operation_id_required")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if !required {
                    return None;
                }
                let full_name = method.full_name();
                let (service, method) = full_name
                    .rsplit_once('.')
                    .expect("method full name includes its service");
                Some(format!("{service}/{method}"))
            })
            .filter(|method| shipped.contains(method.as_str()))
            .collect::<BTreeSet<_>>();
        let catalog = REQUIRED_CLIENT_OPERATION_ID_METHODS
            .iter()
            .map(|method| (*method).to_string())
            .collect::<BTreeSet<_>>();

        assert_eq!(catalog, descriptor_required);
    }

    #[test]
    fn caller_operation_id_is_reused_and_empty_values_get_one_fresh_id() {
        let method = REQUIRED_CLIENT_OPERATION_ID_METHODS[0];
        let supplied = ClientOperationId::caller_or_fresh(method, "stable-1");
        assert_eq!(supplied.as_str(), "stable-1");
        assert_eq!(supplied.to_wire(), supplied.to_wire());

        let generated = ClientOperationId::caller_or_fresh(method, "");
        assert!(!generated.as_str().is_empty());
        assert_eq!(generated.to_wire(), generated.to_wire());
    }
}
