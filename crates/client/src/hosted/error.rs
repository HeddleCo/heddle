use api::heddle::api::v1alpha1::CallFailure;

/// Failure returned by the native hosted-call module.
#[derive(Debug, thiserror::Error)]
pub enum HostedError {
    #[error("hosted endpoint descriptor is invalid: {0}")]
    InvalidDescriptor(String),
    #[error("hosted endpoint descriptor signature is invalid")]
    InvalidDescriptorSignature,
    #[error("hosted endpoint descriptor is expired or not yet valid")]
    DescriptorOutsideValidityWindow,
    #[error("hosted transport failed: {0}")]
    Transport(String),
    #[error("hosted call framing failed: {0}")]
    Framing(String),
    #[error("hosted call failed with {code:?}: {message}")]
    Call {
        code: api::heddle::api::v1alpha1::CallFailureCode,
        message: String,
        details: Vec<prost_types::Any>,
    },
    #[error("hosted protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("hosted request signing failed: {0}")]
    Signing(#[from] crypto::SignerError),
    #[error("hosted request signing requires an authenticated principal")]
    SigningIdentityRequired,
    #[error("hosted bearer capability is not valid UTF-8")]
    InvalidBearerCapability,
    #[error("hosted stream opening requires a canonical repository reference")]
    InvalidRepositoryReference,
    #[error("hosted request is missing its required client_operation_id")]
    MissingClientOperationId,
    #[error("hosted request metadata is invalid: {0}")]
    RequestMetadata(#[from] api::RequestMetadataError),
    #[error("hosted bootstrap HTTPS request failed: {0}")]
    BootstrapHttp(#[from] reqwest::Error),
}

impl HostedError {
    pub(super) fn transport(error: impl std::fmt::Display) -> Self {
        Self::Transport(error.to_string())
    }

    pub(super) fn framing(error: impl std::fmt::Display) -> Self {
        Self::Framing(error.to_string())
    }
}

impl From<CallFailure> for HostedError {
    fn from(failure: CallFailure) -> Self {
        Self::Call {
            code: failure.code(),
            message: failure.message,
            details: failure.details,
        }
    }
}
