//! Portable ordinary-Biscuit evidence for delegated Spool creation.
//!
//! Structural validity is never an admission receipt. Fresh admission uses the
//! actual current owner state and clock, and the host must apply live revocation
//! and account permissions to the returned ordinary capability facts.
use biscuit_auth::{Biscuit, PublicKey, builder::BlockBuilder};
use chrono::{DateTime, Utc};
use prost::Message;

use crate::{
    Error, Result, VerificationLimits, VerifiedOwnerState,
    canonical::{Encoder, digest},
    capability::validate_path_segments,
    crypto::{validate_key, verify_signature},
    owner::{apply_accepted_transition, verify_owner_root},
    wire::*,
};

/// Maximum complete portable creation proof, including owner-history witness.
pub const MAX_CREATION_PROOF_BYTES: usize = 256 * 1024;
/// Signature domain for the exact creator statement.
pub const CREATION_DOMAIN: &[u8] = b"heddle-spool-creation-v1";
/// Signature domain for the owner-to-mint-root association.
pub const MINT_ROOT_DOMAIN: &[u8] = b"heddle-mint-root-attachment-v1";
/// Verifier-only fact that no token block may assert or derive.
pub const CREATION_REQUEST_PREDICATE: &str = "heddle_spool_creation_request_v1";

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
fn required<'a, T>(value: &'a Option<T>, name: &str) -> Result<&'a T> {
    value
        .as_ref()
        .ok_or_else(|| invalid(format!("missing {name}")))
}
fn key(encoder: &mut Encoder, value: &AuthorizationVerificationKey) -> Result<()> {
    validate_key(value)?;
    encoder.i32(value.algorithm);
    encoder.bytes(&value.public_key)
}
fn account(value: &[u8]) -> Result<()> {
    if value.len() != 16 || value.iter().all(|byte| *byte == 0) {
        return Err(invalid("account UUID must be nonzero and 16 bytes"));
    }
    Ok(())
}

/// Canonical digest of the immutable owner/key/Spool binding.
pub fn spool_genesis_digest(genesis: &SpoolOwnerGenesis) -> Result<[u8; 32]> {
    account(&genesis.spool_uuid)?;
    let mut body = Encoder::new();
    body.bytes(&genesis.spool_uuid)?;
    key(
        &mut body,
        required(&genesis.owner_public_key, "genesis owner key")?,
    )?;
    Ok(digest(b"heddle-spool-owner-genesis-v1", &body.finish()))
}

/// Fixed-order canonical creator statement; signatures cover its domain digest.
pub fn canonical_spool_creation(value: &SpoolCreationStatement) -> Result<Vec<u8>> {
    if value.format_version != 1
        || value.genesis_digest.len() != 32
        || value.owner_state_hash.len() != 32
        || value.created_at_unix_seconds <= 0
    {
        return Err(invalid(
            "invalid creation statement version, digest or timestamp",
        ));
    }
    account(&value.account_uuid)?;
    if !value.parent_spool_uuid.is_empty() {
        account(&value.parent_spool_uuid)?;
    }
    if value.parent_spool_uuid.is_empty() != value.parent_path_segments.is_empty() {
        return Err(invalid(
            "parent UUID and canonical path must both be present",
        ));
    }
    validate_path_segments(&value.parent_path_segments)?;
    validate_path_segments(std::slice::from_ref(&value.name))?;
    let mut body = Encoder::new();
    body.u32(value.format_version);
    body.bytes(&value.genesis_digest)?;
    body.bytes(&value.account_uuid)?;
    body.bytes(&value.owner_state_hash)?;
    body.u64(value.owner_sequence);
    key(&mut body, required(&value.creator_key, "creator key")?)?;
    body.bytes(&value.parent_spool_uuid)?;
    body.count(value.parent_path_segments.len())?;
    for segment in &value.parent_path_segments {
        body.string(segment)?;
    }
    body.string(&value.name)?;
    body.i64(value.created_at_unix_seconds);
    Ok(body.finish())
}

/// Canonical owner-signed association; this certifies a mint root, not access.
pub fn canonical_mint_root_attachment(value: &MintRootAttachment) -> Result<Vec<u8>> {
    if value.format_version != 1
        || value.owner_state_hash.len() != 32
        || value.nonce.len() != 32
        || value.not_before_unix_seconds < 0
        || value.expires_at_unix_seconds <= value.not_before_unix_seconds
    {
        return Err(invalid(
            "invalid mint-root attachment version, fields or interval",
        ));
    }
    account(&value.account_uuid)?;
    let mut body = Encoder::new();
    body.u32(value.format_version);
    body.bytes(&value.account_uuid)?;
    body.bytes(&value.owner_state_hash)?;
    body.u64(value.owner_sequence);
    key(
        &mut body,
        required(&value.owner_key, "attachment owner key")?,
    )?;
    key(
        &mut body,
        required(&value.mint_root_key, "attached mint root")?,
    )?;
    body.i64(value.not_before_unix_seconds);
    body.i64(value.expires_at_unix_seconds);
    body.bytes(&value.nonce)?;
    Ok(body.finish())
}

/// Digest to sign for a creator statement.
pub fn spool_creation_signing_digest(value: &SpoolCreationStatement) -> Result<[u8; 32]> {
    Ok(digest(CREATION_DOMAIN, &canonical_spool_creation(value)?))
}
/// Digest to sign with the current owner authority for a mint-root association.
pub fn mint_root_signing_digest(value: &MintRootAttachment) -> Result<[u8; 32]> {
    Ok(digest(
        MINT_ROOT_DOMAIN,
        &canonical_mint_root_attachment(value)?,
    ))
}

/// Verify a mint-root association at a real enrollment or refresh boundary.
/// The independently verified current owner state and expected account/root
/// are caller inputs; the attachment cannot supply its own authority.
pub fn verify_mint_root_attachment(
    signed: &SignedMintRootAttachment,
    current: &VerifiedOwnerState,
    expected_account_uuid: &[u8],
    expected_mint_root_key: &[u8],
    now: i64,
) -> Result<()> {
    let attachment = required(&signed.attachment, "mint-root attachment")?;
    let body = canonical_mint_root_attachment(attachment)?;
    let root = required(&current.signed_root().root, "current owner root")?;
    if attachment.account_uuid != expected_account_uuid
        || attachment.account_uuid != root.account_uuid
        || attachment.owner_state_hash.as_slice() != current.state_hash()
        || attachment.owner_sequence != current.sequence()
        || attachment.owner_key.as_ref() != Some(current.authority_key())
        || required(&attachment.mint_root_key, "mint root")?.public_key != expected_mint_root_key
    {
        return Err(invalid(
            "mint-root attachment differs from expected account, root or current authority",
        ));
    }
    if now < attachment.not_before_unix_seconds || now >= attachment.expires_at_unix_seconds {
        return Err(invalid("mint-root attachment is not currently valid"));
    }
    verify_signature(
        current.authority_key(),
        required(&signed.owner_signature, "mint-root signature")?,
        MINT_ROOT_DOMAIN,
        &body,
    )
}

/// Exact final narrowing block. The creator appends the existing standard
/// same-key PoP delegation fact before sealing; no fresh bearer is minted.
pub fn creation_restrictions(statement: &SpoolCreationStatement) -> Result<BlockBuilder> {
    let hash = hex::encode(spool_creation_signing_digest(statement)?);
    BlockBuilder::new()
        .check(
            format!(
                "check if operation(\"CreateSpool\"), {CREATION_REQUEST_PREDICATE}(\"{hash}\")"
            )
            .as_str(),
        )
        .map_err(|error| invalid(format!("creation restriction: {error}")))
}

fn history_state(history: &OwnerHistory, now: i64) -> Result<VerifiedOwnerState> {
    if history.accepted_transitions.len() > VerificationLimits::MAX_TRANSITIONS {
        return Err(invalid("creation history exceeds transition bound"));
    }
    let mut state = verify_owner_root(required(&history.root, "creation owner root")?)?;
    let limits = VerificationLimits::new(30 * 24 * 60 * 60)?;
    for transition in &history.accepted_transitions {
        state = apply_accepted_transition(&state, transition, now, limits)?;
    }
    if state.state_hash().as_slice() != history.state_hash {
        return Err(invalid("creation history state hash mismatch"));
    }
    Ok(state)
}

/// Structurally verified public evidence. It is deliberately not a fresh
/// admission decision or evidence that the claimed timestamp was accepted.
pub struct ValidatedSpoolCreation {
    state: VerifiedOwnerState,
    token: Biscuit,
    root: PublicKey,
}
impl ValidatedSpoolCreation {
    /// Owner state authenticated by the complete supplied history witness.
    pub fn owner_state(&self) -> &VerifiedOwnerState {
        &self.state
    }
}

/// Validate signatures, lineage, exact intent and sealed public token shape.
/// `now` bounds future owner-history activations; the creator's timestamp does
/// not establish prior admission. Never use this alone to admit a new Spool.
pub fn validate_spool_creation_structure(
    signed: &SignedSpoolOwnerGenesis,
    now: i64,
) -> Result<ValidatedSpoolCreation> {
    if signed.owner_signature.is_some() {
        return Err(invalid(
            "delegated creation cannot carry an owner self-signature",
        ));
    }
    let proof = required(&signed.delegated_creation, "delegated creation proof")?;
    if proof.encoded_len() > MAX_CREATION_PROOF_BYTES
        || proof.sealed_biscuit.is_empty()
        || proof.sealed_biscuit.len() > 64 * 1024
    {
        return Err(invalid("creation proof exceeds its bound"));
    }
    let statement = required(&proof.statement, "creation statement")?;
    let canonical = canonical_spool_creation(statement)?;
    let genesis = required(&signed.genesis, "owner genesis")?;
    if statement.genesis_digest.as_slice() != spool_genesis_digest(genesis)? {
        return Err(invalid("creation proof names another genesis"));
    }
    let creator = required(&statement.creator_key, "creator key")?;
    verify_signature(
        creator,
        required(&proof.creator_signature, "creator signature")?,
        CREATION_DOMAIN,
        &canonical,
    )?;
    let state = history_state(
        required(&proof.owner_history, "creation owner history")?,
        now,
    )?;
    let root_body = required(&state.signed_root().root, "owner root")?;
    if statement.account_uuid != root_body.account_uuid
        || statement.owner_state_hash.as_slice() != state.state_hash()
        || statement.owner_sequence != state.sequence()
        || genesis.owner_public_key.as_ref() != Some(state.authority_key())
    {
        return Err(invalid(
            "creation statement differs from witnessed owner authority",
        ));
    }
    let mint = if let Some(signed_attachment) = &proof.mint_root_attachment {
        let attachment = required(&signed_attachment.attachment, "mint-root attachment")?;
        let body = canonical_mint_root_attachment(attachment)?;
        if attachment.account_uuid != statement.account_uuid
            || attachment.owner_state_hash != statement.owner_state_hash
            || attachment.owner_sequence != statement.owner_sequence
            || attachment.owner_key.as_ref() != Some(state.authority_key())
        {
            return Err(invalid("mint root is attached to another owner state"));
        }
        verify_signature(
            state.authority_key(),
            required(
                &signed_attachment.owner_signature,
                "mint-root owner signature",
            )?,
            MINT_ROOT_DOMAIN,
            &body,
        )?;
        required(&attachment.mint_root_key, "mint root")?
    } else {
        state.authority_key()
    };
    let root = PublicKey::from_bytes(&mint.public_key, biscuit_auth::Algorithm::Ed25519)
        .map_err(|error| invalid(error.to_string()))?;
    let token = Biscuit::from(&proof.sealed_biscuit, root)
        .map_err(|error| invalid(format!("creation credential signature: {error}")))?;
    let effective = heddle_biscuit_verifier::facts::verify_proof_key_lineage(&token)
        .map_err(|error| invalid(format!("creation proof-key lineage: {error}")))?;
    if effective != hex::encode(&creator.public_key) {
        return Err(invalid(
            "creator signature is not rooted in the credential proof-key lineage",
        ));
    }
    if !matches!(token.seal(), Err(biscuit_auth::error::Token::AlreadySealed)) {
        return Err(invalid("public creation evidence must be sealed"));
    }
    if token.block_count() < 2 {
        return Err(invalid("creation token lacks exact narrowing block"));
    }
    for index in 0..token.block_count() {
        let source = token
            .print_block_source(index)
            .map_err(|error| invalid(error.to_string()))?;
        let block = BlockBuilder::new()
            .code(&source)
            .map_err(|error| invalid(error.to_string()))?;
        if block
            .facts
            .iter()
            .any(|fact| fact.predicate.name == CREATION_REQUEST_PREDICATE)
            || block
                .rules
                .iter()
                .any(|rule| rule.head.name == CREATION_REQUEST_PREDICATE)
        {
            return Err(invalid("credential asserts reserved creation request fact"));
        }
        if index + 1 == token.block_count() {
            let exact = creation_restrictions(statement)?;
            if !block.rules.is_empty()
                || !block.scopes.is_empty()
                || block.facts.len() != 1
                || block.facts[0].predicate.name != "pop_delegation"
                || block.checks != exact.checks
            {
                return Err(invalid(
                    "creation token final block differs from exact intent",
                ));
            }
        }
    }
    Ok(ValidatedSpoolCreation { state, token, root })
}

/// Admit a newly presented delegated creation at the actual current time.
/// The caller compares parent UUID/path/name to the resolved request and must
/// check returned session/block revocation identifiers against live host state.
/// A successful return does not persist an admission record on the caller's behalf.
pub fn admit_fresh_spool_creation(
    signed: &SignedSpoolOwnerGenesis,
    current: &VerifiedOwnerState,
    now: i64,
) -> Result<heddle_biscuit_verifier::BiscuitFacts> {
    let validated = validate_spool_creation_structure(signed, now)?;
    if validated.state.state_hash() != current.state_hash()
        || validated.state.signed_root() != current.signed_root()
    {
        return Err(invalid("creation requires the actual current owner state"));
    }
    let proof = required(&signed.delegated_creation, "creation proof")?;
    let statement = required(&proof.statement, "creation statement")?;
    if statement.created_at_unix_seconds > now.saturating_add(30) {
        return Err(invalid("creation timestamp is in the future"));
    }
    if let Some(signed_attachment) = &proof.mint_root_attachment {
        let attachment = required(&signed_attachment.attachment, "mint-root attachment")?;
        if now < attachment.not_before_unix_seconds || now >= attachment.expires_at_unix_seconds {
            return Err(invalid("mint-root attachment is not currently valid"));
        }
    }
    let time =
        DateTime::<Utc>::from_timestamp(now, 0).ok_or_else(|| invalid("invalid admission time"))?;
    let path = statement.parent_path_segments.join("/");
    let resource = if path.is_empty() {
        None
    } else {
        Some(("spool", path.as_str()))
    };
    let fact = format!(
        "{CREATION_REQUEST_PREDICATE}(\"{}\")",
        hex::encode(spool_creation_signing_digest(statement)?)
    );
    let token = validated
        .token
        .to_base64()
        .map_err(|error| invalid(error.to_string()))?;
    let facts = heddle_biscuit_verifier::verify_any_at_with_extra_facts(
        &token,
        None,
        &[validated.root],
        &[],
        "CreateSpool",
        resource,
        &[fact],
        time,
    )
    .map_err(|error| invalid(format!("creation capability: {error}")))?;
    if facts.cnf.as_deref()
        != Some(hex::encode(&required(&statement.creator_key, "creator key")?.public_key).as_str())
    {
        return Err(invalid(
            "creation signer is not the credential's effective proof key",
        ));
    }
    if facts
        .subject_user_id()
        .is_some_and(|account| account.as_bytes().as_slice() != statement.account_uuid)
    {
        return Err(invalid("creation credential belongs to another account"));
    }
    Ok(facts)
}
