use std::path::PathBuf;

/// Failure while constructing, persisting, or verifying owner authorization.
#[derive(Debug, thiserror::Error)]
pub enum AuthorizationError {
    #[error("invalid owner-authorization object: {0}")]
    Invalid(String),
    #[error("owner-authorization signature verification failed")]
    InvalidSignature,
    #[error("owner-authorization chain is broken: {0}")]
    BrokenChain(String),
    #[error("recovery threshold is not satisfied: required {required}, got {actual}")]
    RecoveryThreshold { required: u32, actual: usize },
    #[error("capability does not grant {0}")]
    CapabilityDenied(String),
    #[error("owner-authorization object is expired")]
    Expired,
    #[error("owner-authorization object is not yet valid")]
    NotYetValid,
    #[error("owner-authorization protobuf is not canonical")]
    NonCanonicalProtobuf,
    #[error("owner-authorization keyring exceeds {limit} bytes")]
    KeyringTooLarge { limit: usize },
    #[error("owner-authorization keyring is missing at '{0}'")]
    MissingKeyring(PathBuf),
    #[error("owner-authorization keyring is already pinned at '{0}'")]
    AlreadyPinned(PathBuf),
    #[error("owner-authorization cryptography failed: {0}")]
    Crypto(String),
    #[error("owner-authorization persistence failed: {0}")]
    Persistence(String),
    #[error("owner-authorization Biscuit failed: {0}")]
    Biscuit(String),
    #[error("owner-authorization I/O failed at '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("owner-authorization protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
}

impl From<crypto::SignerError> for AuthorizationError {
    fn from(error: crypto::SignerError) -> Self {
        Self::Crypto(error.to_string())
    }
}

/// Result type for owner-anchored authorization operations.
pub type Result<T> = std::result::Result<T, AuthorizationError>;
