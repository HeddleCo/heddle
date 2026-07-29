use super::key_id;
use crate::owner_authorization::{
    AuthorizationError, Result,
    wire::{AuthorizationVerificationKey, RecoveryGuardian, RecoveryPolicy},
};

pub(super) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(super) fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(super) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    pub(super) fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn bytes(&mut self, value: &[u8]) -> Result<()> {
        let len = u32::try_from(value.len()).map_err(|_| {
            AuthorizationError::Invalid("canonical field exceeds u32 length".to_string())
        })?;
        self.u32(len);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    pub(super) fn string(&mut self, value: &str) -> Result<()> {
        self.bytes(value.as_bytes())
    }

    pub(super) fn count(&mut self, len: usize) -> Result<()> {
        self.u32(u32::try_from(len).map_err(|_| {
            AuthorizationError::Invalid("canonical collection exceeds u32 length".to_string())
        })?);
        Ok(())
    }
}

pub(super) fn verification_key(
    encoder: &mut Encoder,
    key: &AuthorizationVerificationKey,
) -> Result<()> {
    encoder.i32(key.algorithm);
    encoder.bytes(&key.public_key)
}

fn guardian(encoder: &mut Encoder, guardian: &RecoveryGuardian) -> Result<()> {
    encoder.i32(guardian.kind);
    verification_key(
        encoder,
        guardian.key.as_ref().ok_or_else(|| {
            AuthorizationError::Invalid("recovery guardian has no key".to_string())
        })?,
    )
}

pub(super) fn recovery_policy(encoder: &mut Encoder, policy: &RecoveryPolicy) -> Result<()> {
    let mut guardians = policy.guardians.iter().collect::<Vec<_>>();
    guardians.sort_by_key(|guardian| guardian.key.as_ref().map(key_id).unwrap_or([0; 32]));
    if guardians
        .iter()
        .zip(&policy.guardians)
        .any(|(sorted, original)| !std::ptr::eq(*sorted, original))
    {
        return Err(AuthorizationError::Invalid(
            "recovery guardians are not sorted by key id".to_string(),
        ));
    }

    encoder.u32(policy.threshold);
    encoder.count(guardians.len())?;
    for guardian_value in guardians {
        guardian(encoder, guardian_value)?;
    }
    Ok(())
}
