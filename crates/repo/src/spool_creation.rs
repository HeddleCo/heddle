//! Device-side producer for portable delegated creation. The caller supplies
//! the existing credential; this module never mints an independent bearer.
use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::*;
use crypto::Signer;
use heddleco_capability_verifier::creation;
// Device callers share the same verification boundary as portable producers.
pub use heddleco_capability_verifier::{
    creation::admit_fresh_spool_creation, verify_spool_owner_genesis,
};

/// Exact intended parent and address, authenticated alongside the new UUID.
pub struct SpoolCreationIntent {
    pub spool_uuid: uuid::Uuid,
    pub parent_spool_uuid: Option<uuid::Uuid>,
    pub parent_path_segments: Vec<String>,
    pub name: String,
}

/// Sign a public association for a device's independent mint root using the
/// actual current owner authority. Association does not grant CreateSpool.
pub fn sign_mint_root_attachment(
    owner_signer: &impl Signer,
    owner: &OwnerState,
    mint_root_public_key: &[u8],
    not_before: i64,
    expires_at: i64,
    nonce: [u8; 32],
) -> Result<SignedMintRootAttachment> {
    let current = crate::verify_account_owner_observation(owner, not_before)?;
    if current.authority_key().public_key != owner_signer.public_key() {
        bail!("mint-root attachment needs the current owner authority signer");
    }
    let root = current
        .signed_root()
        .root
        .as_ref()
        .context("verified owner root body")?;
    let attachment = MintRootAttachment {
        format_version: 1,
        account_uuid: root.account_uuid.clone(),
        owner_state_hash: current.state_hash().to_vec(),
        owner_sequence: current.sequence(),
        owner_key: Some(current.authority_key().clone()),
        mint_root_key: Some(crate::ed25519_verification_key(mint_root_public_key)?),
        not_before_unix_seconds: not_before,
        expires_at_unix_seconds: expires_at,
        nonce: nonce.to_vec(),
    };
    let signature = crate::sign_canonical(
        owner_signer,
        creation::MINT_ROOT_DOMAIN,
        &creation::canonical_mint_root_attachment(&attachment)?,
    )?;
    Ok(SignedMintRootAttachment {
        attachment: Some(attachment),
        owner_signature: Some(signature),
    })
}

/// Sign and seal an existing capability for exactly this Spool creation.
/// The result passes local fresh verification; Weft independently repeats that
/// check against actual current owner state, permissions, and revocations.
pub fn sign_delegated_spool_creation(
    creator: &impl Signer,
    intent: SpoolCreationIntent,
    owner: &OwnerState,
    existing_biscuit: &str,
    mint_root_attachment: Option<SignedMintRootAttachment>,
    now: i64,
) -> Result<SignedSpoolOwnerGenesis> {
    let current = crate::verify_account_owner_observation(owner, now)?;
    let root = current
        .signed_root()
        .root
        .as_ref()
        .context("owner root body")?;
    if intent.spool_uuid.is_nil() {
        bail!("new Spool UUID is nil");
    }
    let genesis = SpoolOwnerGenesis {
        spool_uuid: intent.spool_uuid.as_bytes().to_vec(),
        owner_public_key: Some(current.authority_key().clone()),
    };
    let statement = SpoolCreationStatement {
        format_version: 1,
        genesis_digest: creation::spool_genesis_digest(&genesis)?.to_vec(),
        account_uuid: root.account_uuid.clone(),
        owner_state_hash: current.state_hash().to_vec(),
        owner_sequence: current.sequence(),
        creator_key: Some(crate::ed25519_verification_key(creator.public_key())?),
        parent_spool_uuid: intent
            .parent_spool_uuid
            .map(|id| id.as_bytes().to_vec())
            .unwrap_or_default(),
        parent_path_segments: intent.parent_path_segments,
        name: intent.name,
        created_at_unix_seconds: now,
    };
    let key: [u8; 32] = creator
        .public_key()
        .try_into()
        .context("creator Ed25519 public key")?;
    let delegation = heddle_biscuit_verifier::key_delegation::statement(existing_biscuit, &key)?;
    let signature: [u8; 64] = creator
        .sign(&delegation)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("creator Ed25519 signature size"))?;
    let narrowed = heddle_biscuit_verifier::key_delegation::append(
        existing_biscuit,
        &key,
        &signature,
        creation::creation_restrictions(&statement)?,
    )?;
    let sealed = biscuit_auth::UnverifiedBiscuit::from_base64(&narrowed)?
        .seal()?
        .to_vec()?;
    let creator_signature = crate::sign_canonical(
        creator,
        creation::CREATION_DOMAIN,
        &creation::canonical_spool_creation(&statement)?,
    )?;
    let signed = SignedSpoolOwnerGenesis {
        genesis: Some(genesis),
        owner_signature: None,
        delegated_creation: Some(SpoolCreationProof {
            statement: Some(statement),
            creator_signature: Some(creator_signature),
            sealed_biscuit: sealed,
            mint_root_attachment,
            owner_history: Some(OwnerHistory {
                root: owner.root.clone(),
                accepted_transitions: owner.accepted_transitions.clone(),
                state_hash: current.state_hash().to_vec(),
            }),
        }),
    };
    creation::admit_fresh_spool_creation(&signed, &current, now)
        .context("verify newly signed delegated creation")?;
    Ok(signed)
}
