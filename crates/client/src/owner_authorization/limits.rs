use crate::owner_authorization::{AuthorizationError, Result};

/// Client-side security ceilings required by offline verification.
///
/// api#55 leaves the numeric TTL values as cutover product inputs. They are
/// therefore mandatory construction inputs here rather than server-provided
/// values or permissive defaults.
#[derive(Clone, Copy, Debug)]
pub struct VerificationLimits {
    max_capability_ttl_seconds: i64,
    max_anonymous_ttl_seconds: i64,
    max_keyring_bytes: usize,
}

impl VerificationLimits {
    /// Construct explicit offline-verification ceilings.
    pub fn new(
        max_capability_ttl_seconds: i64,
        max_anonymous_ttl_seconds: i64,
        max_keyring_bytes: usize,
    ) -> Result<Self> {
        if max_capability_ttl_seconds <= 0
            || max_anonymous_ttl_seconds <= 0
            || max_keyring_bytes == 0
        {
            return Err(AuthorizationError::Invalid(
                "verification limits must all be positive".to_string(),
            ));
        }
        Ok(Self {
            max_capability_ttl_seconds,
            max_anonymous_ttl_seconds,
            max_keyring_bytes,
        })
    }

    /// Maximum owner-capability lifetime and key-handover overlap.
    pub fn max_capability_ttl_seconds(self) -> i64 {
        self.max_capability_ttl_seconds
    }

    /// Maximum anonymous pseudonym lifetime.
    pub fn max_anonymous_ttl_seconds(self) -> i64 {
        self.max_anonymous_ttl_seconds
    }

    /// Maximum serialized clone-keyring size.
    pub fn max_keyring_bytes(self) -> usize {
        self.max_keyring_bytes
    }
}
