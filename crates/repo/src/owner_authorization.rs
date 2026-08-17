// SPDX-License-Identifier: Apache-2.0
//! Pinned owner authorization and offline sidecar verification.

use std::{fs, sync::Arc};

use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::{
    AuthorizationVerificationKey, CloneAuthorizationKeyring, OwnerKeyBinding, SidecarAuthorization,
    SidecarIdentity, SidecarOperationSigningBody, SpoolCapabilityAction,
};
use crypto::Signer;
use heddleco_capability_verifier::{
    Decision, Denial, OperationContext, VerificationLimits, verify_clone_keyring,
    verify_sidecar_authorization,
};
use objects::fs_atomic::write_file_atomic;
use objects::object::StateSignature;
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{HeddleError, Repository};

const OWNER_ANCHOR_FILE: &str = "owner-authorization.bin";
const OWNER_PROTOCOL_VERSION: u32 = 1;
const MAX_CAPABILITY_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum PinnedOwnerAnchor {
    Local {
        stable_owner_uuid: [u8; 16],
        algorithm: String,
        public_key: Vec<u8>,
    },
    Hosted {
        protocol_version: u32,
        stable_owner_uuid: [u8; 16],
        keyring: Vec<u8>,
        initial_key_binding: Vec<u8>,
        registry_keys: Vec<PinnedVerificationKey>,
        current_owner_state_hash: [u8; 32],
        spool_uuid: [u8; 16],
        spool_path_segments: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PinnedVerificationKey {
    algorithm: i32,
    public_key: Vec<u8>,
}

impl From<&AuthorizationVerificationKey> for PinnedVerificationKey {
    fn from(value: &AuthorizationVerificationKey) -> Self {
        Self {
            algorithm: value.algorithm,
            public_key: value.public_key.clone(),
        }
    }
}

impl From<&PinnedVerificationKey> for AuthorizationVerificationKey {
    fn from(value: &PinnedVerificationKey) -> Self {
        Self {
            algorithm: value.algorithm,
            public_key: value.public_key.clone(),
        }
    }
}

impl Repository {
    fn owner_anchor_path(&self) -> std::path::PathBuf {
        self.heddle_dir().join(OWNER_ANCHOR_FILE)
    }

    fn read_owner_anchor(&self) -> Result<PinnedOwnerAnchor> {
        let path = self.owner_anchor_path();
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "read pinned owner authorization anchor '{}'",
                path.display()
            )
        })?;
        rmp_serde::from_slice(&bytes).with_context(|| {
            format!(
                "decode pinned owner authorization anchor '{}'",
                path.display()
            )
        })
    }

    fn write_owner_anchor_tofu(&self, anchor: &PinnedOwnerAnchor) -> Result<()> {
        let path = self.owner_anchor_path();
        if path.exists() {
            let existing = self.read_owner_anchor()?;
            if existing == *anchor {
                return Ok(());
            }
            anyhow::bail!(
                "owner authorization anchor mismatch: '{}' is already pinned; refusing later-operation TOFU",
                path.display()
            );
        }
        let bytes = rmp_serde::to_vec_named(anchor).context("encode pinned owner anchor")?;
        write_file_atomic(&path, &bytes)
            .with_context(|| format!("pin owner authorization anchor '{}'", path.display()))?;
        Ok(())
    }

    fn replace_fresh_clone_anchor(&self, anchor: &PinnedOwnerAnchor) -> Result<()> {
        if !self.refs().list_threads()?.is_empty() || !self.refs().list_markers()?.is_empty() {
            anyhow::bail!("clone owner anchor must be pinned before repository refs");
        }
        let path = self.owner_anchor_path();
        if path.exists() {
            match self.read_owner_anchor()? {
                PinnedOwnerAnchor::Local { .. } => {}
                existing if existing == *anchor => return Ok(()),
                PinnedOwnerAnchor::Hosted { .. } => anyhow::bail!(
                    "owner authorization anchor mismatch: '{}' is already pinned; refusing later-operation TOFU",
                    path.display()
                ),
            }
        }
        let bytes = rmp_serde::to_vec_named(anchor).context("encode cloned owner anchor")?;
        write_file_atomic(&path, &bytes)
            .with_context(|| format!("pin clone owner authorization anchor '{}'", path.display()))
    }

    /// Pin the self-sovereign public owner key for a newly created local repo.
    pub(crate) fn initialize_local_owner_anchor(&self) -> Result<()> {
        let identity = crate::identity::load_or_mint_local(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )
        .context("create local owner signing identity")?;
        let public_key = hex::decode(&identity.public_key).context("decode local owner key")?;
        self.write_owner_anchor_tofu(&PinnedOwnerAnchor::Local {
            stable_owner_uuid: *uuid::Uuid::new_v4().as_bytes(),
            algorithm: "ed25519".to_owned(),
            public_key,
        })
    }

    /// Pin the source repository's public owner anchor while constructing a
    /// fresh local clone. Only the disposable anchor minted by `init` may be
    /// replaced, and only before the clone publishes any refs.
    pub fn pin_local_clone_owner_anchor(&self, source: &Repository) -> Result<()> {
        if !matches!(self.read_owner_anchor()?, PinnedOwnerAnchor::Local { .. }) {
            anyhow::bail!("local clone destination already has a hosted owner anchor");
        }
        let source_anchor = source.read_owner_anchor()?;
        if matches!(source_anchor, PinnedOwnerAnchor::Local { .. }) {
            crate::identity::clone_local_identity(
                &source
                    .heddle_dir()
                    .join(crate::identity::LOCAL_IDENTITY_FILE),
                &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
            )
            .context("copy local clone owner signing identity")?;
        }
        self.replace_fresh_clone_anchor(&source_anchor)
    }

    /// Verify a clone keyring with independently pinned registry roots, then
    /// persist its public owner anchor before any pack or sidecar is accepted.
    pub fn pin_hosted_owner_anchor(
        &self,
        protocol_version: u32,
        stable_owner_uuid: &[u8],
        keyring: &CloneAuthorizationKeyring,
        initial_key_binding: &OwnerKeyBinding,
        trusted_registry_keys: &[AuthorizationVerificationKey],
        now_unix_seconds: i64,
    ) -> Result<()> {
        if protocol_version != OWNER_PROTOCOL_VERSION {
            anyhow::bail!("unsupported owner authorization protocol version {protocol_version}");
        }
        if trusted_registry_keys.is_empty() {
            anyhow::bail!(
                "owner anchor cannot be verified: no independently pinned registry attestation public key is available"
            );
        }
        let stable_owner_uuid: [u8; 16] = stable_owner_uuid.try_into().map_err(|_| {
            anyhow::anyhow!("PullReady stable_owner_uuid must contain exactly 16 bytes")
        })?;
        if keyring.stable_owner_uuid != stable_owner_uuid
            || keyring.initial_key_binding.as_ref() != Some(initial_key_binding)
        {
            anyhow::bail!("PullReady owner anchor fields disagree with the authorization keyring");
        }
        let verified = verify_clone_keyring(
            keyring.clone(),
            now_unix_seconds,
            verifier_limits()?,
            trusted_registry_keys,
            &[],
        )
        .context("verify clone owner authorization keyring")?;
        let current_owner_state_hash = verified.initial_owner_state().state_hash();
        let spool_uuid = keyring
            .spool_uuid
            .as_slice()
            .try_into()
            .expect("canonical verifier accepted the 16-byte spool UUID");
        self.replace_fresh_clone_anchor(&PinnedOwnerAnchor::Hosted {
            protocol_version,
            stable_owner_uuid,
            keyring: keyring.encode_to_vec(),
            initial_key_binding: initial_key_binding.encode_to_vec(),
            registry_keys: trusted_registry_keys
                .iter()
                .map(PinnedVerificationKey::from)
                .collect(),
            current_owner_state_hash,
            spool_uuid,
            spool_path_segments: keyring.canonical_spool_path_segments.clone(),
        })
    }

    /// Verify one sidecar capability exclusively against the clone-pinned
    /// owner anchor. Absence and every verifier denial fail closed.
    pub fn verify_owner_sidecar_authorization(
        &self,
        authorization: Option<&SidecarAuthorization>,
        identity: SidecarIdentity,
        required_actions: &[SpoolCapabilityAction],
        raw_payload: &[u8],
        now_unix_seconds: i64,
    ) -> Result<Decision> {
        let authorization = authorization.ok_or_else(|| {
            HeddleError::InvalidObject("owner sidecar capability is absent".to_owned())
        })?;
        let PinnedOwnerAnchor::Hosted {
            stable_owner_uuid,
            keyring,
            initial_key_binding,
            registry_keys,
            current_owner_state_hash,
            spool_uuid,
            spool_path_segments,
            ..
        } = self.read_owner_anchor()?
        else {
            return Err(HeddleError::InvalidObject(
                "hosted sidecar cannot use a local-creation owner anchor".to_owned(),
            )
            .into());
        };
        let keyring = CloneAuthorizationKeyring::decode(keyring.as_slice())
            .context("decode pinned clone authorization keyring")?;
        let initial_key_binding = OwnerKeyBinding::decode(initial_key_binding.as_slice())
            .context("decode pinned initial owner key binding")?;
        if keyring.stable_owner_uuid != stable_owner_uuid
            || keyring.accepted_state_hash != current_owner_state_hash
            || keyring.spool_uuid != spool_uuid
            || keyring.canonical_spool_path_segments != spool_path_segments
        {
            anyhow::bail!("pinned owner authorization anchor is internally inconsistent");
        }
        let registry_keys = registry_keys
            .iter()
            .map(AuthorizationVerificationKey::from)
            .collect::<Vec<_>>();
        let leaf_capability_id = authorization
            .capability
            .as_ref()
            .and_then(|bundle| bundle.capability_chain.last())
            .and_then(|signed| signed.capability.as_ref())
            .map(|capability| capability.capability_id.clone())
            .unwrap_or_default();
        let body = SidecarOperationSigningBody {
            format_version: 1,
            required_actions: required_actions
                .iter()
                .copied()
                .map(|action| action as i32)
                .collect(),
            spool_uuid: spool_uuid.to_vec(),
            sidecar_identity: Some(identity),
            payload_sha256: Sha256::digest(raw_payload).to_vec(),
            leaf_capability_id,
        };
        let context = OperationContext {
            stable_owner_uuid: &stable_owner_uuid,
            initial_key_binding: &initial_key_binding,
            trusted_registry_keys: &registry_keys,
            current_owner_state_hash: &current_owner_state_hash,
            spool_uuid: &spool_uuid,
            spool_path_segments: &spool_path_segments,
            now_unix_seconds,
            limits: verifier_limits()?,
        };
        let decision = verify_sidecar_authorization(authorization, &body, raw_payload, &context);
        if let Decision::Deny(reason) = decision {
            return Err(HeddleError::InvalidObject(format!(
                "owner sidecar capability denied: {}",
                denial_name(reason)
            ))
            .into());
        }
        let expected = match required_actions {
            [SpoolCapabilityAction::Redact] => Decision::Redact,
            [SpoolCapabilityAction::Redact, SpoolCapabilityAction::Purge] => Decision::Purge,
            [SpoolCapabilityAction::Visibility] => Decision::Visibility,
            [SpoolCapabilityAction::MetadataSupersession] => Decision::MetadataSupersession,
            _ => anyhow::bail!("invalid sidecar action classification"),
        };
        if decision != expected {
            anyhow::bail!("owner sidecar capability returned {decision:?}; expected {expected:?}");
        }
        Ok(decision)
    }

    /// Resolve the private signer corresponding to the local public owner pin.
    pub(crate) fn local_owner_signer(&self) -> Result<Arc<dyn Signer>> {
        let PinnedOwnerAnchor::Local {
            algorithm,
            public_key,
            ..
        } = self.read_owner_anchor()?
        else {
            anyhow::bail!("local operation requires a local-creation owner anchor");
        };
        let signer = crate::identity::load_local_signer(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )
        .ok_or_else(|| anyhow::anyhow!("local owner signing identity is unavailable"))?;
        if !signer.algorithm().eq_ignore_ascii_case(&algorithm) || signer.public_key() != public_key
        {
            anyhow::bail!("local signing identity does not match the pinned owner anchor");
        }
        Ok(Arc::from(signer))
    }

    /// Return the resource UUID from the verified clone-pinned hosted anchor.
    pub fn hosted_owner_spool_uuid(&self) -> Result<[u8; 16]> {
        match self.read_owner_anchor()? {
            PinnedOwnerAnchor::Hosted { spool_uuid, .. } => Ok(spool_uuid),
            PinnedOwnerAnchor::Local { .. } => {
                anyhow::bail!("hosted sidecar requires a clone-pinned hosted owner anchor")
            }
        }
    }

    pub(crate) fn local_owner_authorizes_signature(
        &self,
        signature: &StateSignature,
    ) -> Result<()> {
        let PinnedOwnerAnchor::Local {
            algorithm,
            public_key,
            ..
        } = self.read_owner_anchor()?
        else {
            anyhow::bail!("local metadata requires a local-creation owner anchor");
        };
        if !signature.algorithm.eq_ignore_ascii_case(&algorithm)
            || !signature
                .public_key
                .eq_ignore_ascii_case(&hex::encode(public_key))
        {
            anyhow::bail!("metadata signer does not match the pinned local owner key");
        }
        Ok(())
    }

    /// Sign local authoritative metadata with the key matching the pinned
    /// self-sovereign owner anchor.
    pub fn sign_local_owner_metadata(&self, payload: &[u8]) -> Result<StateSignature> {
        let signer = self.local_owner_signer()?;
        sign_metadata_with(&*signer, payload, "pinned local owner key")
    }

    /// Sign authoritative metadata with the key whose public identity is bound
    /// by the repository owner anchor. Hosted repositories use the protected
    /// device key named by their owner capability.
    pub fn sign_authoritative_metadata(&self, payload: &[u8]) -> Result<StateSignature> {
        let signer = self.authoritative_metadata_signer()?;
        sign_metadata_with(&*signer, payload, "owner-authorized signing key")
    }

    pub(crate) fn authoritative_metadata_signer(&self) -> Result<Arc<dyn Signer>> {
        match self.read_owner_anchor()? {
            PinnedOwnerAnchor::Local { .. } => self.local_owner_signer(),
            PinnedOwnerAnchor::Hosted { .. } => self.signing_signer().ok_or_else(|| {
                anyhow::anyhow!("hosted metadata requires a protected device signing identity")
            }),
        }
    }
}

fn sign_metadata_with(
    signer: &dyn Signer,
    payload: &[u8],
    key_description: &str,
) -> Result<StateSignature> {
    Ok(StateSignature {
        algorithm: signer.algorithm().to_owned(),
        public_key: hex::encode(signer.public_key()),
        signature: hex::encode(
            signer
                .sign(payload)
                .with_context(|| format!("sign metadata with {key_description}"))?,
        ),
    })
}

fn verifier_limits() -> Result<VerificationLimits> {
    VerificationLimits::new(MAX_CAPABILITY_TTL_SECONDS)
        .context("construct owner authorization verifier limits")
}

fn denial_name(denial: Denial) -> &'static str {
    match denial {
        Denial::OverLimit => "over-limit",
        Denial::Malformed => "malformed",
        Denial::InvalidProof => "invalid-proof",
        Denial::OwnerBinding => "owner-binding",
        Denial::StaleOwner => "stale-owner",
        Denial::Capability => "capability",
        Denial::DirectOnly => "direct-only",
        Denial::OperationBinding => "operation-binding",
        Denial::Time => "time",
    }
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha1::{AuthorizationKeyAlgorithm, SidecarOperationSigningBody};
    use heddleco_capability_verifier::{
        conformance::{
            ConformanceCase, ConformanceFixture, FIXTURE_V1_JSON, KEYRING_FIXTURE_V1_JSON,
            KeyringConformanceFixture,
        },
        verify_sidecar_authorization,
    };
    use tempfile::TempDir;

    use super::*;

    struct DecodedCase {
        authorization: SidecarAuthorization,
        body: SidecarOperationSigningBody,
        payload: Vec<u8>,
        binding: OwnerKeyBinding,
        registry_keys: Vec<AuthorizationVerificationKey>,
        stable_owner_uuid: [u8; 16],
        spool_uuid: [u8; 16],
        state_hash: [u8; 32],
    }

    fn direct_cases() -> Vec<ConformanceCase> {
        let fixture: ConformanceFixture = serde_json::from_str(FIXTURE_V1_JSON).unwrap();
        fixture
            .cases
            .into_iter()
            .filter(|case| {
                matches!(
                    case.name.as_str(),
                    "direct-redact"
                        | "direct-purge"
                        | "direct-visibility"
                        | "direct-metadata-supersession"
                )
            })
            .collect()
    }

    fn decode_case(case: &ConformanceCase) -> DecodedCase {
        let authorization =
            SidecarAuthorization::decode(hex::decode(&case.authorization_hex).unwrap().as_slice())
                .unwrap();
        let body = SidecarOperationSigningBody::decode(
            hex::decode(&case.operation_body_hex).unwrap().as_slice(),
        )
        .unwrap();
        let payload = hex::decode(&case.payload_hex).unwrap();
        let binding = OwnerKeyBinding::decode(
            hex::decode(&case.initial_key_binding_hex)
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let registry_keys = case
            .registry_public_keys_hex
            .iter()
            .map(|key| AuthorizationVerificationKey {
                algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
                public_key: hex::decode(key).unwrap(),
            })
            .collect();
        DecodedCase {
            authorization,
            body,
            payload,
            binding,
            registry_keys,
            stable_owner_uuid: hex::decode(&case.stable_owner_uuid_hex)
                .unwrap()
                .try_into()
                .unwrap(),
            spool_uuid: hex::decode(&case.spool_uuid_hex)
                .unwrap()
                .try_into()
                .unwrap(),
            state_hash: hex::decode(&case.current_owner_state_hash_hex)
                .unwrap()
                .try_into()
                .unwrap(),
        }
    }

    fn decide(
        case: &ConformanceCase,
        authorization: &SidecarAuthorization,
        body: &SidecarOperationSigningBody,
        payload: &[u8],
        now: i64,
        spool_uuid_override: Option<[u8; 16]>,
    ) -> Decision {
        let decoded = decode_case(case);
        let requested_spool_uuid = spool_uuid_override.unwrap_or(decoded.spool_uuid);
        verify_sidecar_authorization(
            authorization,
            body,
            payload,
            &OperationContext {
                stable_owner_uuid: &decoded.stable_owner_uuid,
                initial_key_binding: &decoded.binding,
                trusted_registry_keys: &decoded.registry_keys,
                current_owner_state_hash: &decoded.state_hash,
                spool_uuid: &requested_spool_uuid,
                spool_path_segments: &case.spool_path_segments,
                now_unix_seconds: now,
                limits: verifier_limits().unwrap(),
            },
        )
    }

    #[test]
    fn offline_accept_deny_matrix_covers_every_protected_operation() {
        for case in direct_cases() {
            let decoded = decode_case(&case);
            let authorization = decoded.authorization;
            let body = decoded.body;
            let payload = decoded.payload;
            assert_eq!(
                decide(
                    &case,
                    &authorization,
                    &body,
                    &payload,
                    case.now_unix_seconds,
                    None,
                ),
                case.expected,
                "{} valid owner capability",
                case.name
            );

            let mut invalid_signature = authorization.clone();
            invalid_signature
                .operation_signature
                .as_mut()
                .unwrap()
                .signature[0] ^= 1;
            assert_eq!(
                decide(
                    &case,
                    &invalid_signature,
                    &body,
                    &payload,
                    case.now_unix_seconds,
                    None,
                ),
                Decision::Deny(Denial::InvalidProof),
                "{} invalid operation signature",
                case.name
            );

            assert_eq!(
                decide(
                    &case,
                    &authorization,
                    &body,
                    &payload,
                    case.now_unix_seconds + 10_000,
                    None,
                ),
                Decision::Deny(Denial::Time),
                "{} expired capability",
                case.name
            );
            assert_eq!(
                decide(
                    &case,
                    &authorization,
                    &body,
                    &payload,
                    case.now_unix_seconds,
                    Some([0x99; 16]),
                ),
                Decision::Deny(Denial::OperationBinding),
                "{} wrong spool",
                case.name
            );

            let mut wrong_action = body.clone();
            wrong_action.required_actions = vec![SpoolCapabilityAction::Read as i32];
            assert_eq!(
                decide(
                    &case,
                    &authorization,
                    &wrong_action,
                    &payload,
                    case.now_unix_seconds,
                    None,
                ),
                Decision::Deny(Denial::Malformed),
                "{} wrong action/identity shape",
                case.name
            );

            let mut forged_attenuation = authorization.clone();
            forged_attenuation
                .capability
                .as_mut()
                .unwrap()
                .capability_chain
                .last_mut()
                .unwrap()
                .signature
                .as_mut()
                .unwrap()
                .signature[0] ^= 1;
            assert_eq!(
                decide(
                    &case,
                    &forged_attenuation,
                    &body,
                    &payload,
                    case.now_unix_seconds,
                    None,
                ),
                Decision::Deny(Denial::InvalidProof),
                "{} forged attenuation",
                case.name
            );
        }
    }

    #[test]
    fn direct_only_attenuation_is_denied_for_purge_and_metadata() {
        let fixture: ConformanceFixture = serde_json::from_str(FIXTURE_V1_JSON).unwrap();
        for name in [
            "attenuated-purge-direct-only",
            "attenuated-metadata-supersession-direct-only",
        ] {
            let case = fixture.cases.iter().find(|case| case.name == name).unwrap();
            let decoded = decode_case(case);
            assert_eq!(
                decide(
                    case,
                    &decoded.authorization,
                    &decoded.body,
                    &decoded.payload,
                    case.now_unix_seconds,
                    None,
                ),
                Decision::Deny(Denial::DirectOnly),
                "{name}"
            );
        }
    }

    #[test]
    fn local_clone_pins_the_source_owner_and_keeps_its_signer() {
        let source_dir = TempDir::new().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        let destination_dir = TempDir::new().unwrap();
        let destination = Repository::init(destination_dir.path()).unwrap();

        destination.pin_local_clone_owner_anchor(&source).unwrap();

        assert_eq!(
            source.read_owner_anchor().unwrap(),
            destination.read_owner_anchor().unwrap()
        );
        assert_eq!(
            source.local_owner_signer().unwrap().public_key(),
            destination.local_owner_signer().unwrap().public_key()
        );
        destination
            .sign_local_owner_metadata(b"post-clone owner operation")
            .unwrap();
    }

    #[test]
    fn clone_anchor_is_verified_pinned_and_never_replaced_by_a_later_operation() {
        let cases = direct_cases();
        let decoded = decode_case(&cases[0]);
        let binding = decoded.binding;
        let registry_keys = decoded.registry_keys;
        let stable_owner_uuid = decoded.stable_owner_uuid;
        let spool_uuid = decoded.spool_uuid;
        let state_hash = decoded.state_hash;
        let bundle = decoded.authorization.capability.unwrap();
        let owner_root = bundle.owner_root.unwrap();
        let expected_owner_id = owner_root.root.as_ref().unwrap().owner_id.clone();
        let keyring = CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: spool_uuid.to_vec(),
            canonical_spool_path_segments: cases[0].spool_path_segments.clone(),
            pin: Some(api::heddle::api::v1alpha1::CloneOwnerPin {
                kind: api::heddle::api::v1alpha1::CloneOwnerPinKind::CloneTofu as i32,
                expected_owner_id,
                first_seen_unix_seconds: cases[0].now_unix_seconds,
            }),
            owner_root: Some(owner_root),
            accepted_transitions: bundle.owner_state_chain,
            accepted_state_hash: state_hash.to_vec(),
            public_access_capabilities: Vec::new(),
            stable_owner_uuid: stable_owner_uuid.to_vec(),
            initial_key_binding: Some(binding.clone()),
            ownership_transfers: Vec::new(),
        };

        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        repo.pin_hosted_owner_anchor(
            1,
            &keyring.stable_owner_uuid,
            &keyring,
            &binding,
            &registry_keys,
            cases[0].now_unix_seconds,
        )
        .unwrap();

        for case in cases {
            let decoded = decode_case(&case);
            let identity = decoded.body.sidecar_identity.clone().unwrap();
            let actions = decoded
                .body
                .required_actions
                .iter()
                .map(|value| SpoolCapabilityAction::try_from(*value).unwrap())
                .collect::<Vec<_>>();
            let absent = repo
                .verify_owner_sidecar_authorization(
                    None,
                    identity.clone(),
                    &actions,
                    &decoded.payload,
                    case.now_unix_seconds,
                )
                .unwrap_err();
            assert!(absent.to_string().contains("capability is absent"));
            assert_eq!(
                repo.verify_owner_sidecar_authorization(
                    Some(&decoded.authorization),
                    identity.clone(),
                    &actions,
                    &decoded.payload,
                    case.now_unix_seconds,
                )
                .unwrap(),
                case.expected
            );

            let mut attacker = decoded.authorization;
            let root_key = attacker
                .capability
                .as_mut()
                .unwrap()
                .owner_root
                .as_mut()
                .unwrap()
                .root
                .as_mut()
                .unwrap()
                .authority_key
                .as_mut()
                .unwrap();
            root_key.public_key[0] ^= 1;
            let error = repo
                .verify_owner_sidecar_authorization(
                    Some(&attacker),
                    identity,
                    &actions,
                    &decoded.payload,
                    case.now_unix_seconds,
                )
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("owner sidecar capability denied")
            );
        }

        let replacement = PinnedOwnerAnchor::Hosted {
            protocol_version: 1,
            stable_owner_uuid: [0x55; 16],
            keyring: keyring.encode_to_vec(),
            initial_key_binding: binding.encode_to_vec(),
            registry_keys: registry_keys
                .iter()
                .map(PinnedVerificationKey::from)
                .collect(),
            current_owner_state_hash: keyring.accepted_state_hash.as_slice().try_into().unwrap(),
            spool_uuid: keyring.spool_uuid.as_slice().try_into().unwrap(),
            spool_path_segments: keyring.canonical_spool_path_segments,
        };
        let error = repo.write_owner_anchor_tofu(&replacement).unwrap_err();
        assert!(error.to_string().contains("refusing later-operation TOFU"));
    }

    #[test]
    fn clone_anchor_fails_closed_without_registry_release_root() {
        let fixture: KeyringConformanceFixture =
            serde_json::from_str(KEYRING_FIXTURE_V1_JSON).unwrap();
        let case = fixture
            .cases
            .iter()
            .find(|case| case.expected_accept)
            .unwrap();
        let keyring =
            CloneAuthorizationKeyring::decode(hex::decode(&case.keyring_hex).unwrap().as_slice())
                .unwrap();
        let binding = keyring.initial_key_binding.clone().unwrap();
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let error = repo
            .pin_hosted_owner_anchor(
                1,
                &keyring.stable_owner_uuid,
                &keyring,
                &binding,
                &[],
                case.now_unix_seconds,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no independently pinned registry attestation public key")
        );
        assert!(matches!(
            repo.read_owner_anchor().unwrap(),
            PinnedOwnerAnchor::Local { .. }
        ));
    }
}
