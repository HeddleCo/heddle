// SPDX-License-Identifier: Apache-2.0
//! Clone-pinned owner genesis and canonical offline purge verification.

use std::fs;

use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::{
    AuthorizationKeyAlgorithm, AuthorizationSignature, AuthorizationVerificationKey,
    PurgeOperationSigningBody, PurgeSidecarIdentity, SidecarAuthorization, SignedSpoolOwnerGenesis,
    SpoolOwnerGenesis,
};
use crypto::Signer;
use heddleco_capability_verifier::{
    Decision, PurgeContext, VerificationLimits, verify_authorization_bundle,
    verify_purge_authorization, verify_spool_owner_genesis,
};
use objects::{fs_atomic::write_file_atomic, lock::RepositoryLockExt};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{HeddleError, Repository};

const OWNER_GENESIS_PIN_FILE: &str = "owner-authorization.bin";
const OWNER_AUTHORIZATION_PROTOCOL_VERSION: u32 = 2;
const MAX_CAPABILITY_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
const OWNER_KEY_ID_DOMAIN: &[u8] = b"heddle-key-v1";

/// Protocol-2 self-signature: the owner key signs `SHA-256(public_key || uuid)`.
///
/// CreateSpool callers mint this locally. Weft only verifies the
/// self-signature and takes the new spool UUID from the genesis; it has no
/// owner private key. Use a UUIDv7 — weft checks the version.
pub fn sign_spool_owner_genesis(
    signer: &impl Signer,
    spool_uuid: [u8; 16],
) -> Result<SignedSpoolOwnerGenesis, crypto::SignerError> {
    let owner_public_key = AuthorizationVerificationKey {
        algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
        public_key: signer.public_key().to_vec(),
    };
    let mut key_id_body = Vec::with_capacity(4 + owner_public_key.public_key.len());
    key_id_body.extend_from_slice(&owner_public_key.algorithm.to_be_bytes());
    key_id_body.extend_from_slice(&owner_public_key.public_key);
    let signer_key_id = Sha256::new()
        .chain_update(OWNER_KEY_ID_DOMAIN)
        .chain_update(key_id_body)
        .finalize()
        .to_vec();
    let digest = Sha256::new()
        .chain_update(&owner_public_key.public_key)
        .chain_update(spool_uuid)
        .finalize();
    Ok(SignedSpoolOwnerGenesis {
        genesis: Some(SpoolOwnerGenesis {
            spool_uuid: spool_uuid.to_vec(),
            owner_public_key: Some(owner_public_key),
        }),
        owner_signature: Some(AuthorizationSignature {
            signer_key_id,
            signature: signer.sign(&digest)?,
        }),
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PinnedOwnerGenesis {
    protocol_version: u32,
    spool_uuid: [u8; 16],
    owner_public_key: Vec<u8>,
    canonical_spool_path_segments: Vec<String>,
    signed_genesis: Vec<u8>,
}

impl Repository {
    fn owner_genesis_pin_path(&self) -> std::path::PathBuf {
        self.heddle_dir().join(OWNER_GENESIS_PIN_FILE)
    }

    fn read_owner_genesis_pin(&self) -> Result<PinnedOwnerGenesis> {
        let path = self.owner_genesis_pin_path();
        let bytes = fs::read(&path)
            .with_context(|| format!("read owner genesis pin '{}'", path.display()))?;
        let pin: PinnedOwnerGenesis = rmp_serde::from_slice(&bytes)
            .with_context(|| format!("decode owner genesis pin '{}'", path.display()))?;
        if pin.protocol_version != OWNER_AUTHORIZATION_PROTOCOL_VERSION {
            anyhow::bail!(
                "unsupported pinned owner authorization protocol version {}",
                pin.protocol_version
            );
        }
        let signed = decode_canonical_genesis(&pin.signed_genesis)?;
        let verified = verify_spool_owner_genesis(&signed)
            .context("verify persisted self-signed owner genesis")?;
        if verified.spool_uuid() != pin.spool_uuid
            || verified.owner_public_key().public_key != pin.owner_public_key
        {
            anyhow::bail!("persisted owner genesis pin is internally inconsistent");
        }
        Ok(pin)
    }

    /// Verify the self-signed `PullReady` genesis and TOFU-pin its exact
    /// spool-to-owner-key binding before accepting any pack or sidecar bytes.
    pub fn verify_and_pin_owner_genesis(
        &self,
        protocol_version: u32,
        signed: Option<&SignedSpoolOwnerGenesis>,
        canonical_spool_path_segments: &[String],
    ) -> Result<()> {
        if protocol_version != OWNER_AUTHORIZATION_PROTOCOL_VERSION {
            anyhow::bail!("unsupported owner authorization protocol version {protocol_version}");
        }
        let signed = signed.ok_or_else(|| {
            HeddleError::InvalidObject("PullReady owner genesis is absent".to_owned())
        })?;
        let verified = verify_spool_owner_genesis(signed)
            .context("verify PullReady self-signed owner genesis")?;
        let candidate = PinnedOwnerGenesis {
            protocol_version,
            spool_uuid: verified.spool_uuid(),
            owner_public_key: verified.owner_public_key().public_key.clone(),
            canonical_spool_path_segments: canonical_spool_path_segments.to_vec(),
            signed_genesis: signed.encode_to_vec(),
        };

        let _lock = self.locker().write()?;
        let path = self.owner_genesis_pin_path();
        if path.exists() {
            let pinned = self.read_owner_genesis_pin()?;
            if pinned.spool_uuid != candidate.spool_uuid
                || pinned.owner_public_key != candidate.owner_public_key
                || pinned.canonical_spool_path_segments != candidate.canonical_spool_path_segments
            {
                anyhow::bail!(
                    "owner genesis mismatch: '{}' is already TOFU-pinned; refusing first-operation trust",
                    path.display()
                );
            }
            return Ok(());
        }
        let bytes = rmp_serde::to_vec_named(&candidate).context("encode owner genesis pin")?;
        write_file_atomic(&path, &bytes)
            .with_context(|| format!("TOFU-pin owner genesis '{}'", path.display()))
    }

    /// Verify a purge authorization against the clone-pinned genesis and the
    /// complete owner-signed root/transition chain carried by its evidence.
    pub fn verify_owner_purge_authorization(
        &self,
        blob_hash: &str,
        raw_payload: &[u8],
        authorization: Option<&SidecarAuthorization>,
        now_unix_seconds: i64,
    ) -> Result<()> {
        let authorization = authorization.ok_or_else(|| {
            HeddleError::InvalidObject("owner purge capability is absent".to_owned())
        })?;
        let pin = self.read_owner_genesis_pin()?;
        let signed_genesis = decode_canonical_genesis(&pin.signed_genesis)?;
        let limits = verifier_limits()?;
        let bundle = authorization.capability.as_ref().ok_or_else(|| {
            HeddleError::InvalidObject("owner purge capability is absent".to_owned())
        })?;
        let verified_bundle = verify_authorization_bundle(bundle, now_unix_seconds, limits)
            .context("verify owner purge transition and capability chain")?;
        let leaf_capability_id = verified_bundle
            .capability()
            .capability()
            .capability_id
            .clone();
        let current_owner_state_hash = verified_bundle.owner_state().state_hash();
        let body = PurgeOperationSigningBody {
            format_version: OWNER_AUTHORIZATION_PROTOCOL_VERSION,
            spool_uuid: pin.spool_uuid.to_vec(),
            purge_identity: Some(PurgeSidecarIdentity {
                blob_hash: blob_hash.to_owned(),
            }),
            payload_sha256: Sha256::digest(raw_payload).to_vec(),
            leaf_capability_id,
        };
        let decision = verify_purge_authorization(
            authorization,
            &body,
            raw_payload,
            &PurgeContext {
                owner_genesis: &signed_genesis,
                current_owner_state_hash: &current_owner_state_hash,
                spool_uuid: &pin.spool_uuid,
                spool_path_segments: &pin.canonical_spool_path_segments,
                now_unix_seconds,
                limits,
            },
        );
        match decision {
            Decision::Purge => Ok(()),
            Decision::Deny(reason) => Err(HeddleError::InvalidObject(format!(
                "owner purge capability denied: {reason:?}"
            ))
            .into()),
        }
    }
}

fn decode_canonical_genesis(bytes: &[u8]) -> Result<SignedSpoolOwnerGenesis> {
    let signed = SignedSpoolOwnerGenesis::decode(bytes)
        .context("decode canonical self-signed owner genesis")?;
    if signed.encode_to_vec() != bytes {
        anyhow::bail!("owner genesis protobuf is not canonical");
    }
    Ok(signed)
}

fn verifier_limits() -> Result<VerificationLimits> {
    VerificationLimits::new(MAX_CAPABILITY_TTL_SECONDS)
        .context("construct owner authorization verifier limits")
}
