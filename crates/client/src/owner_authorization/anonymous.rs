use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, Result, VerificationLimits,
    canonical::{
        ANONYMOUS_CREDENTIAL_DOMAIN, ANONYMOUS_ID_DOMAIN, ANONYMOUS_REGISTRATION_DOMAIN,
        anonymous_body, digest, key_id, nonce, registration_body,
    },
    key::verify_signature,
    wire::{AnonymousKeyCredential, AuthorizationKeyAlgorithm, RegisterAnonymousKeyRequest},
};

/// Create a self-signed anonymous pseudonym under the explicit TTL ceiling.
pub fn create_anonymous_credential(
    key: &AuthorizationKey,
    issued_at_unix_seconds: i64,
    expires_at_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<AnonymousKeyCredential> {
    if issued_at_unix_seconds < 0
        || expires_at_unix_seconds <= issued_at_unix_seconds
        || expires_at_unix_seconds.saturating_sub(issued_at_unix_seconds)
            > limits.max_anonymous_ttl_seconds()
    {
        return Err(AuthorizationError::Invalid(
            "anonymous credential exceeds the TTL ceiling".to_string(),
        ));
    }
    let public = key.verification_key();
    let anonymous_id = digest(ANONYMOUS_ID_DOMAIN, &key_id(&public)).to_vec();
    let mut credential = AnonymousKeyCredential {
        format_version: 1,
        anonymous_id,
        key: Some(public),
        issued_at_unix_seconds,
        expires_at_unix_seconds,
        nonce: nonce(),
        self_signature: None,
    };
    credential.self_signature =
        Some(key.sign(ANONYMOUS_CREDENTIAL_DOMAIN, &anonymous_body(&credential)?)?);
    Ok(credential)
}

/// Verify an anonymous pseudonym and its self-signature offline.
pub fn verify_anonymous_credential(
    credential: &AnonymousKeyCredential,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<()> {
    if credential.format_version != 1
        || credential.anonymous_id.len() != 32
        || credential.nonce.len() != 32
        || credential.issued_at_unix_seconds < 0
        || credential.expires_at_unix_seconds <= credential.issued_at_unix_seconds
        || credential
            .expires_at_unix_seconds
            .saturating_sub(credential.issued_at_unix_seconds)
            > limits.max_anonymous_ttl_seconds()
    {
        return Err(AuthorizationError::Invalid(
            "anonymous credential has invalid v1 fields".to_string(),
        ));
    }
    if now_unix_seconds < credential.issued_at_unix_seconds
        || now_unix_seconds > credential.expires_at_unix_seconds
    {
        return Err(AuthorizationError::Expired);
    }
    let key = credential.key.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("anonymous credential has no key".to_string())
    })?;
    if key.algorithm != AuthorizationKeyAlgorithm::Ed25519 as i32
        || key.public_key.len() != 32
        || credential.anonymous_id.as_slice() != digest(ANONYMOUS_ID_DOMAIN, &key_id(key))
    {
        return Err(AuthorizationError::Invalid(
            "anonymous id or key is invalid".to_string(),
        ));
    }
    verify_signature(
        key,
        credential
            .self_signature
            .as_ref()
            .ok_or(AuthorizationError::InvalidSignature)?,
        ANONYMOUS_CREDENTIAL_DOMAIN,
        &anonymous_body(credential)?,
    )
}

/// Create a registration request whose continuity proof requires the key.
pub fn create_anonymous_registration(
    credential: AnonymousKeyCredential,
    key: &AuthorizationKey,
    turnstile_token: Option<String>,
    prior_continuity_token: String,
    client_operation_id: String,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<RegisterAnonymousKeyRequest> {
    verify_anonymous_credential(&credential, now_unix_seconds, limits)?;
    if credential
        .key
        .as_ref()
        .is_none_or(|public| key.key_id() != key_id(public))
        || client_operation_id.is_empty()
    {
        return Err(AuthorizationError::Invalid(
            "anonymous registration key or operation id is invalid".to_string(),
        ));
    }
    let mut request = RegisterAnonymousKeyRequest {
        credential: Some(credential),
        turnstile_token,
        prior_continuity_token,
        continuity_proof: None,
        client_operation_id,
    };
    request.continuity_proof =
        Some(key.sign(ANONYMOUS_REGISTRATION_DOMAIN, &registration_body(&request)?)?);
    Ok(request)
}

/// Verify anonymous registration continuity without trusting its token.
pub fn verify_anonymous_registration(
    request: &RegisterAnonymousKeyRequest,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<()> {
    let credential = request.credential.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("anonymous registration has no credential".to_string())
    })?;
    verify_anonymous_credential(credential, now_unix_seconds, limits)?;
    if request.client_operation_id.is_empty() {
        return Err(AuthorizationError::Invalid(
            "anonymous registration has no operation id".to_string(),
        ));
    }
    verify_signature(
        credential.key.as_ref().expect("verified credential key"),
        request
            .continuity_proof
            .as_ref()
            .ok_or(AuthorizationError::InvalidSignature)?,
        ANONYMOUS_REGISTRATION_DOMAIN,
        &registration_body(request)?,
    )
}
