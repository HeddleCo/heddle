use wire::ProtocolError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClientOperationId(String);

impl ClientOperationId {
    pub(crate) fn fresh(method: &str) -> Self {
        let path = if method.starts_with('/') {
            method.to_string()
        } else {
            format!("/{method}")
        };
        let required = operation_id_required(&path)
            .unwrap_or_else(|| panic!("unknown hosted method {method}"));
        assert!(
            required,
            "{method} is not declared to require a client operation ID"
        );
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub(crate) fn caller_or_fresh(method: &str, value: impl Into<String>) -> Self {
        let value = value.into();
        if value.is_empty() {
            Self::fresh(method)
        } else {
            Self::for_required_method(method, value)
                .unwrap_or_else(|error| panic!("invalid operation-id method: {error}"))
        }
    }

    pub(crate) fn for_required_method(
        method: &str,
        value: impl Into<String>,
    ) -> Result<Self, ProtocolError> {
        let path = if method.starts_with('/') {
            method.to_string()
        } else {
            format!("/{method}")
        };
        let required = operation_id_required(&path).ok_or_else(|| {
            ProtocolError::InvalidState(format!("unknown hosted method {method}"))
        })?;
        if !required {
            return Err(ProtocolError::InvalidState(format!(
                "{method} is not declared to require a client operation ID"
            )));
        }
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolError::InvalidState(format!(
                "{method} requires a non-empty client operation ID"
            )));
        }
        Ok(Self(scoped_to_method(&path, &value)))
    }

    pub(crate) fn to_wire(&self) -> String {
        self.0.clone()
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn operation_id_required(path: &str) -> Option<bool> {
    api::method_descriptor(path)
        .map(|descriptor| descriptor.client_operation_id_required)
        .or_else(|| {
            api::v2::method_descriptor(path)
                .map(|descriptor| descriptor.client_operation_id_required)
        })
}

/// Derive a per-method UUID so one CLI `--op-id` can drive Open, PutContext,
/// and Publish without weft `DedupOutcome::Conflict`. Retries of the same
/// method still collide on the same derived id.
fn scoped_to_method(method: &str, caller: &str) -> String {
    match uuid::Uuid::parse_str(caller.trim()) {
        Ok(caller) => uuid::Uuid::new_v5(&caller, method.as_bytes()).to_string(),
        Err(_) => uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("{method}\0{caller}").as_bytes(),
        )
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: &str = "heddle.api.v1alpha2.CollaborationService/OpenDiscussion";
    const PUT_CONTEXT: &str = "heddle.api.v1alpha2.CollaborationService/PutContext";
    const PUBLISH: &str = "heddle.api.v1alpha2.SyncService/PublishContent";

    #[test]
    fn caller_uuid_is_scoped_per_method_and_stable_for_retries() {
        let caller = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef).to_string();
        let open = ClientOperationId::caller_or_fresh(OPEN, caller.clone());
        let again = ClientOperationId::caller_or_fresh(OPEN, caller.clone());
        let context = ClientOperationId::caller_or_fresh(PUT_CONTEXT, caller.clone());
        let publish = ClientOperationId::caller_or_fresh(PUBLISH, caller);
        assert_eq!(open, again);
        assert_ne!(open.as_str(), context.as_str());
        assert_ne!(open.as_str(), publish.as_str());
        assert_ne!(context.as_str(), publish.as_str());
        assert!(uuid::Uuid::parse_str(open.as_str()).is_ok());
        assert!(uuid::Uuid::parse_str(context.as_str()).is_ok());
    }
}
