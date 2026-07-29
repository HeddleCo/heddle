use std::collections::BTreeSet;

use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, Result,
    canonical::key_id,
    wire::{AuthorizationKeyAlgorithm, RecoveryGuardian, RecoveryGuardianKind, RecoveryPolicy},
};

/// Dedicated recovery signer and its signed guardian provenance.
pub struct GuardianSigner {
    kind: RecoveryGuardianKind,
    key: AuthorizationKey,
}

impl GuardianSigner {
    /// Construct a paper-kit guardian.
    pub fn paper(key: AuthorizationKey) -> Self {
        Self {
            kind: RecoveryGuardianKind::Paper,
            key,
        }
    }

    /// Construct a social guardian.
    pub fn social(key: AuthorizationKey) -> Self {
        Self {
            kind: RecoveryGuardianKind::Social,
            key,
        }
    }

    /// Construct an opt-in Weft guardian.
    pub fn weft(key: AuthorizationKey) -> Self {
        Self {
            kind: RecoveryGuardianKind::Weft,
            key,
        }
    }

    /// Borrow the dedicated signing key.
    pub fn key(&self) -> &AuthorizationKey {
        &self.key
    }
}

/// Explicit acknowledgement required to construct custodial Weft-only recovery.
pub struct CustodialRecoveryConfirmation {
    _private: (),
}

/// Confirm that a 1-of-1 Weft policy makes the account custodial.
#[must_use]
pub fn confirm_custodial_weft_only() -> CustodialRecoveryConfirmation {
    CustodialRecoveryConfirmation { _private: () }
}

/// Validated guardian threshold plus the private signers used for setup proofs.
pub struct RecoverySetup {
    threshold: u32,
    guardians: Vec<GuardianSigner>,
}

impl RecoverySetup {
    /// Construct the default two-of-N recovery policy.
    pub fn recommended(guardians: Vec<GuardianSigner>) -> Result<Self> {
        Self::with_threshold(2, guardians)
    }

    /// Construct a non-custodial threshold policy.
    pub fn with_threshold(threshold: u32, guardians: Vec<GuardianSigner>) -> Result<Self> {
        if threshold < 2 {
            return Err(AuthorizationError::Invalid(
                "recovery threshold below 2 requires the explicit Weft-only confirmation path"
                    .to_string(),
            ));
        }
        let setup = Self {
            threshold,
            guardians,
        };
        setup.validate(None, false)?;
        Ok(setup)
    }

    /// Construct the explicitly confirmed 1-of-1 custodial Weft policy.
    pub fn custodial_weft_only(
        guardian: GuardianSigner,
        _confirmation: CustodialRecoveryConfirmation,
    ) -> Result<Self> {
        if guardian.kind != RecoveryGuardianKind::Weft {
            return Err(AuthorizationError::Invalid(
                "custodial recovery requires exactly one Weft guardian".to_string(),
            ));
        }
        let setup = Self {
            threshold: 1,
            guardians: vec![guardian],
        };
        setup.validate(None, true)?;
        Ok(setup)
    }

    /// Threshold committed to the owner state.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Guardian signers used for root and policy possession proofs.
    pub fn guardians(&self) -> &[GuardianSigner] {
        &self.guardians
    }

    pub(crate) fn to_wire(&self, authority: &AuthorizationKey) -> Result<RecoveryPolicy> {
        self.validate(Some(&authority.key_id()), self.threshold == 1)?;
        let mut guardians = self
            .guardians
            .iter()
            .map(|guardian| RecoveryGuardian {
                kind: guardian.kind as i32,
                key: Some(guardian.key.verification_key()),
            })
            .collect::<Vec<_>>();
        guardians.sort_by_key(|guardian| {
            key_id(guardian.key.as_ref().expect("constructed guardian key"))
        });
        Ok(RecoveryPolicy {
            threshold: self.threshold,
            guardians,
        })
    }

    fn validate(&self, authority_key_id: Option<&[u8; 32]>, allow_custodial: bool) -> Result<()> {
        if self.threshold == 0 || self.threshold as usize > self.guardians.len() {
            return Err(AuthorizationError::Invalid(
                "recovery threshold exceeds the guardian set".to_string(),
            ));
        }
        if self.threshold < 2
            && (!allow_custodial
                || self.guardians.len() != 1
                || self.guardians[0].kind != RecoveryGuardianKind::Weft)
        {
            return Err(AuthorizationError::Invalid(
                "1-of-1 recovery is allowed only for explicitly confirmed Weft custody".to_string(),
            ));
        }
        if self
            .guardians
            .iter()
            .any(|guardian| guardian.kind == RecoveryGuardianKind::Weft)
            && self.threshold >= 2
            && !self.guardians.iter().any(|guardian| {
                matches!(
                    guardian.kind,
                    RecoveryGuardianKind::Paper | RecoveryGuardianKind::Social
                )
            })
        {
            return Err(AuthorizationError::Invalid(
                "a Weft guardian requires a paper or social co-factor".to_string(),
            ));
        }

        let mut ids = BTreeSet::new();
        for guardian in &self.guardians {
            let id = guardian.key.key_id();
            if authority_key_id == Some(&id) || !ids.insert(id) {
                return Err(AuthorizationError::Invalid(
                    "authority and recovery guardian keys must be distinct".to_string(),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_wire_policy(
    policy: &RecoveryPolicy,
    authority_key_id: &[u8; 32],
    allow_empty: bool,
) -> Result<()> {
    if allow_empty && policy.threshold == 0 && policy.guardians.is_empty() {
        return Ok(());
    }
    if policy.threshold == 0 || policy.threshold as usize > policy.guardians.len() {
        return Err(AuthorizationError::Invalid(
            "recovery threshold is outside the guardian set".to_string(),
        ));
    }
    let custodial = policy.threshold == 1
        && policy.guardians.len() == 1
        && policy.guardians[0].kind == RecoveryGuardianKind::Weft as i32;
    if policy.threshold < 2 && !custodial {
        return Err(AuthorizationError::Invalid(
            "wire policy has an unapproved threshold below 2".to_string(),
        ));
    }
    let has_weft = policy
        .guardians
        .iter()
        .any(|guardian| guardian.kind == RecoveryGuardianKind::Weft as i32);
    let has_independent_cofactor = policy.guardians.iter().any(|guardian| {
        matches!(
            RecoveryGuardianKind::try_from(guardian.kind),
            Ok(RecoveryGuardianKind::Paper | RecoveryGuardianKind::Social)
        )
    });
    if has_weft && !custodial && !has_independent_cofactor {
        return Err(AuthorizationError::Invalid(
            "Weft recovery lacks a paper or social co-factor".to_string(),
        ));
    }

    let mut ids = Vec::with_capacity(policy.guardians.len());
    for guardian in &policy.guardians {
        RecoveryGuardianKind::try_from(guardian.kind)
            .ok()
            .filter(|kind| *kind != RecoveryGuardianKind::Unspecified)
            .ok_or_else(|| {
                AuthorizationError::Invalid("unknown recovery guardian kind".to_string())
            })?;
        let key = guardian.key.as_ref().ok_or_else(|| {
            AuthorizationError::Invalid("recovery guardian has no key".to_string())
        })?;
        if key.algorithm != AuthorizationKeyAlgorithm::Ed25519 as i32 || key.public_key.len() != 32
        {
            return Err(AuthorizationError::Invalid(
                "recovery guardian key is not 32-byte Ed25519".to_string(),
            ));
        }
        let id = key_id(key);
        if &id == authority_key_id {
            return Err(AuthorizationError::Invalid(
                "authority key cannot also be a recovery guardian".to_string(),
            ));
        }
        ids.push(id);
    }
    if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(AuthorizationError::Invalid(
            "recovery guardians must be unique and sorted by key id".to_string(),
        ));
    }
    Ok(())
}
