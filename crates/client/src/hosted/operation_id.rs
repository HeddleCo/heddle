use wire::ProtocolError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ClientOperationId(String);

impl ClientOperationId {
    pub(super) fn fresh(method: &str) -> Self {
        let path = if method.starts_with('/') {
            method.to_string()
        } else {
            format!("/{method}")
        };
        let descriptor = api::method_descriptor(&path)
            .unwrap_or_else(|| panic!("unknown hosted method {method}"));
        assert!(
            descriptor.client_operation_id_required,
            "{method} is not declared to require a client operation ID"
        );
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub(super) fn caller_or_fresh(method: &str, value: impl Into<String>) -> Self {
        let value = value.into();
        if value.is_empty() {
            Self::fresh(method)
        } else {
            Self::for_required_method(method, value)
                .unwrap_or_else(|error| panic!("invalid operation-id method: {error}"))
        }
    }

    pub(super) fn for_required_method(
        method: &str,
        value: impl Into<String>,
    ) -> Result<Self, ProtocolError> {
        let path = if method.starts_with('/') {
            method.to_string()
        } else {
            format!("/{method}")
        };
        let descriptor = api::method_descriptor(&path).ok_or_else(|| {
            ProtocolError::InvalidState(format!("unknown hosted method {method}"))
        })?;
        if !descriptor.client_operation_id_required {
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
        Ok(Self(value))
    }

    pub(super) fn to_wire(&self) -> String {
        self.0.clone()
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}
