// SPDX-License-Identifier: Apache-2.0
//! Client mint for protocol-1 owner roots and ClaimDeferredHuman.
//!
//! Weft verifies; it does not hold the owner private key. Sequence-0 of a
//! claimable deferred-human root is the same device/proof key CreateSpool
//! pins as spool genesis. Claim advances authority with ClaimDeferredHuman
//! and must not mint a replacement human sequence-0.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{
    AuthorizationKeyAlgorithm, AuthorizationSignature, AuthorizationVerificationKey,
    OwnerKeyBinding, OwnerKeyBindingKind, OwnerKeyTransition, OwnerKeyTransitionKind, OwnerRoot,
    RecoveryPolicy, SignedOwnerKeyTransition, SignedOwnerRoot, SignedSpoolOwnerGenesis,
    SpoolOwnerGenesis,
};
use crypto::{Signer, SignerError, verify_payload_signature};
use heddleco_capability_verifier::{
    VerificationLimits, apply_transition, verify_owner_key_binding, verify_owner_root,
    verify_spool_owner_genesis,
};
use sha2::{Digest, Sha256};

const OWNER_KEY_ID_DOMAIN: &[u8] = b"heddle-key-v1";
const OWNER_ROOT_DOMAIN: &[u8] = b"heddle-owner-root-v1";
/// Domain separator shared by Heddle, tapestry, and weft for owner transitions.
pub const OWNER_TRANSITION_DOMAIN: &[u8] = b"heddle-owner-key-transition-v1";
const OWNER_BINDING_DOMAIN: &[u8] = b"heddle-owner-key-binding-v1";
const REGISTRATION_BINDING_NONCE_DOMAIN: &[u8] = b"heddle-owner-registration-binding-nonce-v1";
const OWNER_ROOT_FORMAT_VERSION: u32 = 1;
const DEFAULT_RECOVERY_WINDOW_SECS: u64 = 604_800;

/// Claim window for an agent-rooted deferred human root.
pub const CLAIMABLE_DEFERRED_HUMAN_TTL_SECS: i64 = 90 * 24 * 60 * 60;

/// Protocol-2 self-signature: the owner key signs `SHA-256(public_key || uuid)`.
///
/// CreateSpool callers mint this locally. Use a UUIDv7 — weft checks the version.
pub fn sign_spool_owner_genesis(
    signer: &impl Signer,
    spool_uuid: [u8; 16],
) -> Result<SignedSpoolOwnerGenesis, SignerError> {
    let owner_public_key = ed25519_verification_key(signer.public_key())?;
    let signer_key_id = authorization_key_id(&owner_public_key).to_vec();
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

/// Protocol-1 claimable deferred-human owner root.
///
/// Authority is the caller's device/proof key. Recovery is empty: the verifier
/// allows that only while `claimable_deferred_human` is set. `nonce` must be
/// 32 random bytes. `now_unix_seconds` plus [`CLAIMABLE_DEFERRED_HUMAN_TTL_SECS`]
/// is the claim deadline.
pub fn sign_claimable_deferred_human_root(
    signer: &impl Signer,
    account_uuid: [u8; 16],
    nonce: [u8; 32],
    now_unix_seconds: i64,
) -> Result<SignedOwnerRoot> {
    if now_unix_seconds <= 0 {
        bail!("claimable owner-root clock must be a positive unix timestamp");
    }
    let claimable_until_unix_seconds = now_unix_seconds
        .checked_add(CLAIMABLE_DEFERRED_HUMAN_TTL_SECS)
        .context("claimable owner-root deadline overflows")?;
    let authority_key = ed25519_verification_key(signer.public_key())?;
    let mut root = OwnerRoot {
        format_version: OWNER_ROOT_FORMAT_VERSION,
        owner_id: Vec::new(),
        account_uuid: account_uuid.to_vec(),
        authority_key: Some(authority_key),
        recovery_policy: Some(RecoveryPolicy {
            threshold: 0,
            guardians: Vec::new(),
            window_secs: None,
        }),
        claimable_deferred_human: true,
        nonce: nonce.to_vec(),
        claimable_until_unix_seconds,
    };
    root.owner_id = domain_digest(OWNER_ROOT_DOMAIN, &owner_root_without_id(&root)?).to_vec();
    let body = owner_root_body(&root)?;
    let authority_proof = sign_canonical(signer, OWNER_ROOT_DOMAIN, &body)?;
    let signed = SignedOwnerRoot {
        root: Some(root),
        authority_proof: Some(authority_proof),
        recovery_key_proofs: Vec::new(),
    };
    verify_owner_root(&signed).context("minted claimable owner root failed local verify")?;
    Ok(signed)
}

/// Browser proof material for a ClaimDeferredHuman transition.
///
/// Sequence-0 stays the agent key. The browser owns the next authority and
/// guardian private keys; this device receives only their public material and
/// signatures over the canonical transition body.
pub struct ClaimDeferredHuman<'a, C> {
    pub current_authority: &'a C,
    pub signed_root: &'a SignedOwnerRoot,
    pub next_authority_key: AuthorizationVerificationKey,
    pub next_authority_key_proof: AuthorizationSignature,
    pub next_recovery_policy: RecoveryPolicy,
    pub next_recovery_key_proofs: Vec<AuthorizationSignature>,
    pub valid_from_unix_seconds: i64,
    pub nonce: [u8; 32],
}

/// Co-sign ClaimDeferredHuman with the sequence-0 agent proof key.
///
/// The human authority and every next guardian proof are verified before the
/// agent key signs. The transition is rebuilt from the verified sequence-0
/// root, so browser-supplied owner ids, state hashes, or sequences never cross
/// this seam. The complete co-signed transition is then applied locally with
/// the shared verifier before it may be sent to weft.
pub fn sign_claim_deferred_human<C>(
    claim: ClaimDeferredHuman<'_, C>,
) -> Result<SignedOwnerKeyTransition>
where
    C: Signer,
{
    let state = verify_owner_root(claim.signed_root).context("verify claimable sequence-0 root")?;
    let root = claim
        .signed_root
        .root
        .as_ref()
        .context("signed owner root has no body")?;
    let current_key = root
        .authority_key
        .as_ref()
        .context("signed owner root has no authority")?;
    if current_key.public_key != claim.current_authority.public_key() {
        bail!("claim current authority is not the sequence-0 device/proof key");
    }
    if claim.next_authority_key.public_key == claim.current_authority.public_key() {
        bail!("claim next authority must be the human device root, not the agent key");
    }
    let transition = claim_deferred_human_transition(
        claim.signed_root,
        claim.next_authority_key,
        claim.next_recovery_policy,
        claim.valid_from_unix_seconds,
        claim.nonce,
    )?;
    let body = owner_key_transition_body(&transition)?;
    verify_browser_claim_proofs(
        &transition,
        &claim.next_authority_key_proof,
        &claim.next_recovery_key_proofs,
        &body,
    )?;
    let current_proof = sign_canonical(claim.current_authority, OWNER_TRANSITION_DOMAIN, &body)?;
    let signed = SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: vec![current_proof],
        next_authority_key_proof: Some(claim.next_authority_key_proof),
        next_recovery_key_proofs: claim.next_recovery_key_proofs,
    };
    let limits = VerificationLimits::new(MAX_CAPABILITY_TTL_SECONDS)
        .context("construct claim transition verifier limits")?;
    apply_transition(&state, &signed, claim.valid_from_unix_seconds, limits)
        .context("minted ClaimDeferredHuman failed local verify")?;
    Ok(signed)
}

/// Assemble the canonical sequence-0-to-human transition a browser signs.
///
/// The device calls the same builder again while co-signing; any browser body
/// that substituted the owner id, previous state hash, or sequence therefore
/// produces proofs that fail verification.
pub fn claim_deferred_human_transition(
    signed_root: &SignedOwnerRoot,
    next_authority_key: AuthorizationVerificationKey,
    next_recovery_policy: RecoveryPolicy,
    valid_from_unix_seconds: i64,
    nonce: [u8; 32],
) -> Result<OwnerKeyTransition> {
    let state = verify_owner_root(signed_root).context("verify claimable sequence-0 root")?;
    let root = signed_root
        .root
        .as_ref()
        .context("signed owner root has no body")?;
    if state.sequence() != 0 || !root.claimable_deferred_human {
        bail!("claim must bind to a claimable sequence-0 owner root");
    }
    if valid_from_unix_seconds <= 0 {
        bail!("claim clock must be a positive unix timestamp");
    }
    if valid_from_unix_seconds > root.claimable_until_unix_seconds {
        bail!("claim valid_from exceeds the sequence-0 claimable deadline");
    }
    Ok(OwnerKeyTransition {
        format_version: OWNER_ROOT_FORMAT_VERSION,
        owner_id: state.owner_id().to_vec(),
        previous_state_hash: state.state_hash().to_vec(),
        sequence: 1,
        kind: OwnerKeyTransitionKind::ClaimDeferredHuman as i32,
        next_authority_key: Some(next_authority_key),
        next_recovery_policy: Some(next_recovery_policy),
        valid_from_unix_seconds,
        previous_key_valid_until_unix_seconds: 0,
        nonce: nonce.to_vec(),
    })
}

/// Sequence-0 authority public key from a verified owner root.
pub fn seq0_authority_public_key(signed: &SignedOwnerRoot) -> Result<&[u8]> {
    let root = signed
        .root
        .as_ref()
        .context("signed owner root has no body")?;
    let key = root
        .authority_key
        .as_ref()
        .context("signed owner root has no authority")?;
    Ok(key.public_key.as_slice())
}

/// CreateSpool genesis owner public key.
pub fn genesis_owner_public_key(signed: &SignedSpoolOwnerGenesis) -> Result<&[u8]> {
    let genesis = signed
        .genesis
        .as_ref()
        .context("signed spool genesis has no body")?;
    let key = genesis
        .owner_public_key
        .as_ref()
        .context("signed spool genesis has no owner key")?;
    Ok(key.public_key.as_slice())
}

/// Nonce weft hashes as `registration_binding_nonce(client_operation_id)`.
pub fn registration_binding_nonce(client_operation_id: &str) -> [u8; 32] {
    Sha256::new()
        .chain_update(REGISTRATION_BINDING_NONCE_DOMAIN)
        .chain_update(client_operation_id.as_bytes())
        .finalize()
        .into()
}

/// Self-asserted AgentClaim binding of the claimable seq-0 root to the account UUID.
///
/// `challenge_nonce` is [`registration_binding_nonce`]. The proof must verify
/// with `verify_owner_key_binding` against the same root.
pub fn sign_agent_claim_binding(
    signer: &impl Signer,
    signed_root: &SignedOwnerRoot,
    client_operation_id: &str,
) -> Result<OwnerKeyBinding> {
    let state = verify_owner_root(signed_root).context("verify claimable sequence-0 root")?;
    let root = signed_root
        .root
        .as_ref()
        .context("signed owner root has no body")?;
    let account_uuid: [u8; 16] = root
        .account_uuid
        .as_slice()
        .try_into()
        .context("owner root account_uuid must be 16 bytes")?;
    if root
        .authority_key
        .as_ref()
        .is_none_or(|key| key.public_key != signer.public_key())
    {
        bail!("owner-key binding signer is not the sequence-0 device/proof key");
    }
    let mut binding = OwnerKeyBinding {
        format_version: OWNER_ROOT_FORMAT_VERSION,
        stable_owner_uuid: account_uuid.to_vec(),
        root_public_key: Some(ed25519_verification_key(signer.public_key())?),
        root_state_hash: state.state_hash().to_vec(),
        kind: OwnerKeyBindingKind::AgentClaim as i32,
        binding_epoch: 1,
        challenge_nonce: registration_binding_nonce(client_operation_id).to_vec(),
        root_proof_of_possession: None,
    };
    let body = owner_binding_body(&binding)?;
    binding.root_proof_of_possession = Some(sign_canonical(signer, OWNER_BINDING_DOMAIN, &body)?);
    verify_owner_key_binding(&binding, &state, &account_uuid)
        .context("minted AgentClaim binding failed local verify")?;
    Ok(binding)
}

/// Refuse CreateSpool genesis whose owner key is not the stored sequence-0 key.
pub fn require_genesis_matches_seq0(
    genesis: &SignedSpoolOwnerGenesis,
    seq0_public_key: &[u8],
) -> Result<()> {
    let verified =
        verify_spool_owner_genesis(genesis).context("verify CreateSpool owner genesis")?;
    if verified.owner_public_key().public_key != seq0_public_key {
        bail!(
            "CreateSpool genesis owner key does not match the account sequence-0 owner root; refusing to pin a different key"
        );
    }
    Ok(())
}

/// Ed25519 verification key for a 32-byte public key.
pub fn ed25519_verification_key(
    public_key: &[u8],
) -> Result<AuthorizationVerificationKey, SignerError> {
    if public_key.len() != 32 {
        return Err(SignerError::InvalidKey(format!(
            "authorization public key must be 32 bytes, got {}",
            public_key.len()
        )));
    }
    Ok(AuthorizationVerificationKey {
        algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
        public_key: public_key.to_vec(),
    })
}

/// SHA-256("heddle-key-v1" || algorithm || public_key).
pub fn authorization_key_id(key: &AuthorizationVerificationKey) -> [u8; 32] {
    let mut body = Vec::with_capacity(4 + key.public_key.len());
    body.extend_from_slice(&key.algorithm.to_be_bytes());
    body.extend_from_slice(&key.public_key);
    domain_digest(OWNER_KEY_ID_DOMAIN, &body)
}

/// Sign a domain-separated canonical body the way the verifier checks it.
pub fn sign_canonical(
    signer: &impl Signer,
    domain: &[u8],
    body: &[u8],
) -> Result<AuthorizationSignature, SignerError> {
    let key = ed25519_verification_key(signer.public_key())?;
    Ok(AuthorizationSignature {
        signer_key_id: authorization_key_id(&key).to_vec(),
        signature: signer.sign(&domain_digest(domain, body))?,
    })
}

const MAX_CAPABILITY_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

fn verify_browser_claim_proofs(
    transition: &OwnerKeyTransition,
    next_authority_key_proof: &AuthorizationSignature,
    next_recovery_key_proofs: &[AuthorizationSignature],
    body: &[u8],
) -> Result<()> {
    verify_canonical_signature(
        transition
            .next_authority_key
            .as_ref()
            .context("transition has no next authority key")?,
        next_authority_key_proof,
        OWNER_TRANSITION_DOMAIN,
        body,
    )
    .context("verify browser human authority proof")?;
    let policy = transition
        .next_recovery_policy
        .as_ref()
        .context("transition has no next recovery policy")?;
    if next_recovery_key_proofs.len() != policy.guardians.len() {
        bail!(
            "claim guardian proof count {} does not match next recovery policy {}",
            next_recovery_key_proofs.len(),
            policy.guardians.len()
        );
    }
    for (guardian, proof) in policy.guardians.iter().zip(next_recovery_key_proofs) {
        verify_canonical_signature(
            guardian
                .key
                .as_ref()
                .context("next recovery guardian has no key")?,
            proof,
            OWNER_TRANSITION_DOMAIN,
            body,
        )
        .context("verify browser next guardian proof")?;
    }
    Ok(())
}

fn verify_canonical_signature(
    key: &AuthorizationVerificationKey,
    proof: &AuthorizationSignature,
    domain: &[u8],
    body: &[u8],
) -> Result<()> {
    if key.algorithm != AuthorizationKeyAlgorithm::Ed25519 as i32 {
        bail!("claim proof key must use Ed25519");
    }
    if proof.signer_key_id != authorization_key_id(key) {
        bail!("claim proof signer key id does not match its public key");
    }
    verify_payload_signature(
        &domain_digest(domain, body),
        "ed25519",
        &key.public_key,
        &proof.signature,
    )
    .context("claim proof signature is invalid")
}

fn domain_digest(domain: &[u8], body: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(domain)
        .chain_update(body)
        .finalize()
        .into()
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) -> Result<()> {
        let len = u32::try_from(value.len()).context("canonical collection exceeds u32 length")?;
        self.u32(len);
        self.raw(value);
        Ok(())
    }
}

fn verification_key(encoder: &mut Encoder, key: &AuthorizationVerificationKey) -> Result<()> {
    encoder.i32(key.algorithm);
    encoder.bytes(&key.public_key)
}

fn guardian(
    encoder: &mut Encoder,
    guardian: &api::heddle::api::v1alpha1::RecoveryGuardian,
) -> Result<()> {
    encoder.i32(guardian.kind);
    let key = guardian
        .key
        .as_ref()
        .context("recovery guardian has no key")?;
    verification_key(encoder, key)
}

fn recovery_policy(encoder: &mut Encoder, policy: &RecoveryPolicy) -> Result<()> {
    let ids = policy
        .guardians
        .iter()
        .map(|value| {
            value
                .key
                .as_ref()
                .map(authorization_key_id)
                .unwrap_or([0; 32])
        })
        .collect::<Vec<_>>();
    if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("recovery guardians are not unique and sorted by key id");
    }
    encoder.u32(policy.threshold);
    encoder
        .u32(u32::try_from(policy.guardians.len()).context("guardian count exceeds u32 length")?);
    for value in &policy.guardians {
        guardian(encoder, value)?;
    }
    encoder.u64(policy.window_secs.unwrap_or(DEFAULT_RECOVERY_WINDOW_SECS));
    Ok(())
}

fn owner_binding_body(binding: &OwnerKeyBinding) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(binding.format_version);
    encoder.bytes(&binding.stable_owner_uuid)?;
    verification_key(
        &mut encoder,
        binding
            .root_public_key
            .as_ref()
            .context("OwnerKeyBinding.root_public_key")?,
    )?;
    encoder.bytes(&binding.root_state_hash)?;
    encoder.i32(binding.kind);
    encoder.u64(binding.binding_epoch);
    encoder.bytes(&binding.challenge_nonce)?;
    Ok(encoder.finish())
}

fn owner_root_without_id(root: &OwnerRoot) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(root.format_version);
    encoder.bytes(&root.account_uuid)?;
    verification_key(
        &mut encoder,
        root.authority_key
            .as_ref()
            .context("OwnerRoot.authority_key")?,
    )?;
    recovery_policy(
        &mut encoder,
        root.recovery_policy
            .as_ref()
            .context("OwnerRoot.recovery_policy")?,
    )?;
    encoder.bool(root.claimable_deferred_human);
    encoder.bytes(&root.nonce)?;
    encoder.i64(root.claimable_until_unix_seconds);
    Ok(encoder.finish())
}

fn owner_root_body(root: &OwnerRoot) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(root.format_version);
    encoder.bytes(&root.owner_id)?;
    encoder.bytes(&root.account_uuid)?;
    verification_key(
        &mut encoder,
        root.authority_key
            .as_ref()
            .context("OwnerRoot.authority_key")?,
    )?;
    recovery_policy(
        &mut encoder,
        root.recovery_policy
            .as_ref()
            .context("OwnerRoot.recovery_policy")?,
    )?;
    encoder.bool(root.claimable_deferred_human);
    encoder.bytes(&root.nonce)?;
    encoder.i64(root.claimable_until_unix_seconds);
    Ok(encoder.finish())
}

/// Canonical transition body signed by owner authorities and recovery keys.
///
/// The field order and counted byte encoding are shared with tapestry's
/// `transitionBody` and weft's owner-transition verifier.
pub fn owner_key_transition_body(transition: &OwnerKeyTransition) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(transition.format_version);
    encoder.bytes(&transition.owner_id)?;
    encoder.bytes(&transition.previous_state_hash)?;
    encoder.u64(transition.sequence);
    encoder.i32(transition.kind);
    verification_key(
        &mut encoder,
        transition
            .next_authority_key
            .as_ref()
            .context("OwnerKeyTransition.next_authority_key")?,
    )?;
    recovery_policy(
        &mut encoder,
        transition
            .next_recovery_policy
            .as_ref()
            .context("OwnerKeyTransition.next_recovery_policy")?,
    )?;
    encoder.i64(transition.valid_from_unix_seconds);
    encoder.i64(transition.previous_key_valid_until_unix_seconds);
    encoder.bytes(&transition.nonce)?;
    Ok(encoder.finish())
}
