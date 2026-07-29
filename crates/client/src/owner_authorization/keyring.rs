use std::{
    fs,
    path::{Path, PathBuf},
};

use prost::Message;

use crate::owner_authorization::{
    AuthorizationError, Result, VerificationLimits, VerifiedCapability, VerifiedOwnerState,
    apply_transition,
    canonical::key_id,
    capability::{request_matches_selector, validate_path_segments, verify_subject_biscuit},
    keyring_verification::verify_clone_keyring,
    root::verify_owner_root,
    wire::{
        AuthorizationVerificationKey, CapabilityPrincipalKind, CloneAuthorizationKeyring,
        CloneOwnerPin, CloneOwnerPinKind, SignedOwnerCapability, SignedOwnerKeyTransition,
        SignedOwnerRoot, SpoolCapabilityAction,
    },
};

const KEYRING_RELATIVE_PATH: &str = "owner-authorization/keyring.pb";

/// A request evaluated solely from persisted clone bytes.
pub struct OfflineRequest {
    /// Subject category.
    pub subject_kind: CapabilityPrincipalKind,
    /// Signed audit identity.
    pub principal_id: Vec<u8>,
    /// Subject public key, absent only for `ANY_ANONYMOUS`.
    pub subject_key: Option<AuthorizationVerificationKey>,
    /// Subject-signed Biscuit bytes.
    pub subject_biscuit: Vec<u8>,
    /// Complete canonical path segments inside the root spool.
    pub path_segments: Vec<String>,
    /// Literal action being requested.
    pub action: SpoolCapabilityAction,
    /// Local verifier time.
    pub now_unix_seconds: i64,
}

/// Keyring after its pin, owner chain, state hash, and public policy verify.
pub struct VerifiedCloneKeyring {
    pub(super) wire: CloneAuthorizationKeyring,
    pub(super) owner_state: VerifiedOwnerState,
    pub(super) capabilities: Vec<VerifiedCapability>,
}

impl VerifiedCloneKeyring {
    /// Accepted owner state.
    pub fn owner_state(&self) -> &VerifiedOwnerState {
        &self.owner_state
    }

    /// Exact persisted transport object.
    pub fn wire(&self) -> &CloneAuthorizationKeyring {
        &self.wire
    }
}

/// Filesystem persistence for a clone's public owner keyring.
pub struct CloneKeyringStore {
    path: PathBuf,
    limits: VerificationLimits,
}

impl CloneKeyringStore {
    /// Locate the dormant keyring under a clone's real `.heddle` directory.
    pub fn new(heddle_dir: impl AsRef<Path>, limits: VerificationLimits) -> Self {
        Self {
            path: heddle_dir.as_ref().join(KEYRING_RELATIVE_PATH),
            limits,
        }
    }

    /// Exact persistence path, intentionally separate from legacy trust keys.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Verify and atomically persist a keyring.
    pub fn install(
        &self,
        keyring: CloneAuthorizationKeyring,
        now_unix_seconds: i64,
    ) -> Result<VerifiedCloneKeyring> {
        let verified = verify_clone_keyring(keyring, now_unix_seconds, self.limits)?;
        if self
            .path
            .try_exists()
            .map_err(|source| AuthorizationError::Io {
                path: self.path.clone(),
                source,
            })?
        {
            return Err(AuthorizationError::AlreadyPinned(self.path.clone()));
        }
        self.persist(&verified)?;
        Ok(verified)
    }

    fn persist(&self, verified: &VerifiedCloneKeyring) -> Result<()> {
        let bytes = verified.wire.encode_to_vec();
        if bytes.len() > self.limits.max_keyring_bytes() {
            return Err(AuthorizationError::KeyringTooLarge {
                limit: self.limits.max_keyring_bytes(),
            });
        }
        let parent = self.path.parent().expect("keyring has parent");
        fs::create_dir_all(parent).map_err(|source| AuthorizationError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        objects::fs_atomic::write_file_atomic(&self.path, &bytes)
            .map_err(|error| AuthorizationError::Persistence(error.to_string()))?;
        Ok(())
    }

    /// Load and reverify the complete keyring without consulting a remote.
    pub fn load(&self, now_unix_seconds: i64) -> Result<VerifiedCloneKeyring> {
        let metadata = fs::metadata(&self.path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                AuthorizationError::MissingKeyring(self.path.clone())
            } else {
                AuthorizationError::Io {
                    path: self.path.clone(),
                    source,
                }
            }
        })?;
        if metadata.len() > self.limits.max_keyring_bytes() as u64 {
            return Err(AuthorizationError::KeyringTooLarge {
                limit: self.limits.max_keyring_bytes(),
            });
        }
        let bytes = fs::read(&self.path).map_err(|source| AuthorizationError::Io {
            path: self.path.clone(),
            source,
        })?;
        let keyring = CloneAuthorizationKeyring::decode(bytes.as_slice())?;
        if keyring.encode_to_vec() != bytes {
            return Err(AuthorizationError::NonCanonicalProtobuf);
        }
        verify_clone_keyring(keyring, now_unix_seconds, self.limits)
    }

    /// Append a verified linear transition suffix and persist it atomically.
    pub fn append_transitions(
        &self,
        transitions: &[SignedOwnerKeyTransition],
        now_unix_seconds: i64,
    ) -> Result<VerifiedCloneKeyring> {
        let current = self.load(now_unix_seconds)?;
        let mut wire = current.wire;
        let mut state = current.owner_state;
        for transition in transitions {
            state = apply_transition(&state, transition, now_unix_seconds, self.limits)?;
            wire.accepted_transitions.push(transition.clone());
        }
        wire.accepted_state_hash = state.state_hash().to_vec();
        let verified = verify_clone_keyring(wire, now_unix_seconds, self.limits)?;
        self.persist(&verified)?;
        Ok(verified)
    }
}

/// Construct a clone keyring from an externally authenticated owner fingerprint.
#[allow(clippy::too_many_arguments)]
pub fn create_clone_keyring(
    spool_uuid: [u8; 16],
    canonical_spool_path_segments: Vec<String>,
    pin_kind: CloneOwnerPinKind,
    expected_owner_id: [u8; 32],
    first_seen_unix_seconds: i64,
    owner_root: SignedOwnerRoot,
    accepted_transitions: Vec<SignedOwnerKeyTransition>,
    public_access_capabilities: Vec<SignedOwnerCapability>,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<CloneAuthorizationKeyring> {
    let mut state = verify_owner_root(&owner_root)?;
    for transition in &accepted_transitions {
        state = apply_transition(&state, transition, now_unix_seconds, limits)?;
    }
    let keyring = CloneAuthorizationKeyring {
        format_version: 1,
        spool_uuid: spool_uuid.to_vec(),
        canonical_spool_path_segments,
        pin: Some(CloneOwnerPin {
            kind: pin_kind as i32,
            expected_owner_id: expected_owner_id.to_vec(),
            first_seen_unix_seconds,
        }),
        owner_root: Some(owner_root),
        accepted_transitions,
        accepted_state_hash: state.state_hash().to_vec(),
        public_access_capabilities,
    };
    verify_clone_keyring(keyring.clone(), now_unix_seconds, limits)?;
    Ok(keyring)
}

/// Offline capability evaluator backed only by one verified clone keyring.
pub struct OfflineAuthorizer {
    keyring: VerifiedCloneKeyring,
}

impl OfflineAuthorizer {
    /// Consume a verified keyring; no network handle is accepted or retained.
    pub fn new(keyring: VerifiedCloneKeyring) -> Self {
        Self { keyring }
    }

    /// Return `Ok(())` only for a literal matching subject, scope, action, and time.
    pub fn authorize(&self, request: &OfflineRequest) -> Result<()> {
        validate_path_segments(&request.path_segments)?;
        if request.action == SpoolCapabilityAction::Unspecified
            || request.subject_kind == CapabilityPrincipalKind::Unspecified
            || !request
                .path_segments
                .starts_with(&self.keyring.wire.canonical_spool_path_segments)
        {
            return Err(AuthorizationError::CapabilityDenied(
                "request is outside the pinned clone".to_string(),
            ));
        }
        let spool_uuid: [u8; 16] = self
            .keyring
            .wire
            .spool_uuid
            .as_slice()
            .try_into()
            .expect("verified spool UUID");
        for verified in &self.keyring.capabilities {
            let capability = verified.capability();
            if request.now_unix_seconds < capability.not_before_unix_seconds
                || request.now_unix_seconds > capability.expires_at_unix_seconds
            {
                continue;
            }
            let subject = capability.subject.as_ref().expect("verified subject");
            if subject.kind != request.subject_kind as i32
                || subject.principal_id != request.principal_id
                || subject.key.as_ref().map(key_id) != request.subject_key.as_ref().map(key_id)
            {
                continue;
            }
            verify_subject_biscuit(capability, &request.subject_biscuit)?;
            if capability.grants.iter().any(|grant| {
                grant.actions.contains(&(request.action as i32))
                    && grant.spool.as_ref().is_some_and(|selector| {
                        request_matches_selector(selector, &spool_uuid, &request.path_segments)
                    })
            }) {
                return Ok(());
            }
        }
        Err(AuthorizationError::CapabilityDenied(format!(
            "{:?} on {}",
            request.action,
            request.path_segments.join("/")
        )))
    }
}
