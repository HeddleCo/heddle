//! Signature-verified metadata extraction without implying operation authority.
use biscuit_auth::{Biscuit, PublicKey};

use crate::{BiscuitError, BiscuitFacts, authorizer_limits};

/// Cryptographically authenticated metadata, deliberately not an authorization
/// result. No RPC/check permission is inferred from these inspection fields.
pub struct InspectedCredential {
    pub asserted_account: Option<uuid::Uuid>,
    pub agent_id: Option<String>,
    pub proof_public_key: Vec<u8>,
    pub expires_at_unix_seconds: u64,
    pub revocation_ids: Vec<String>,
    pub session_id: String,
    pub device_id: Option<String>,
    pub credential_id: Option<String>,
}
impl InspectedCredential {
    /// All revocation selectors; inspection reports identity, not permission.
    pub fn revocation_identities(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.session_id.as_str())
            .chain(self.credential_id.as_deref())
            .chain(self.revocation_ids.iter().map(String::as_str))
            .filter(|id| !id.is_empty())
    }
}
/// Inspect an already signature-verified Biscuit without requiring it to permit
/// an introspection RPC. The bounded Datalog fixpoint and authority-scoped fact
/// extractor verify the complete effective proof-key chain; checks are evaluated
/// separately by normal authorization whenever any operation is attempted.
pub fn inspect_verified_credential(
    biscuit: &Biscuit,
    root: &PublicKey,
) -> Result<InspectedCredential, BiscuitError> {
    let mut authorizer = biscuit_auth::builder::AuthorizerBuilder::new()
        .set_limits(authorizer_limits())
        .build(biscuit)
        .map_err(|e| BiscuitError::Invalid(e.to_string()))?;
    authorizer
        .run()
        .map_err(|e| BiscuitError::Invalid(e.to_string()))?;
    let facts = BiscuitFacts::extract(
        &mut authorizer,
        biscuit,
        Some(&hex::encode(root.to_bytes())),
        &[],
    )?;
    let key = facts
        .cnf
        .as_ref()
        .and_then(|key| hex::decode(key).ok())
        .filter(|key| key.len() == 32)
        .ok_or_else(|| BiscuitError::Invalid("verified proof key missing".into()))?;
    let agent_id = facts.delegation_agent_id.clone().or_else(|| {
        (facts.agent_provider.is_some() || facts.agent_model.is_some()).then(|| facts.sid.clone())
    });
    Ok(InspectedCredential {
        agent_id,
        asserted_account: facts.subject_user_id(),
        proof_public_key: key,
        expires_at_unix_seconds: facts.exp,
        revocation_ids: facts.revocation_ids,
        session_id: facts.sid,
        device_id: facts.device_id,
        credential_id: facts.credential_id,
    })
}
#[cfg(test)]
mod tests {
    use biscuit_auth::{
        KeyPair, PrivateKey,
        builder::{Algorithm, BlockBuilder},
    };
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    fn fixture() -> (Biscuit, PublicKey) {
        let key =
            KeyPair::from(&PrivateKey::from_bytes(&[17; 32], Algorithm::Ed25519).expect("root"));
        let token=Biscuit::builder().code(format!("user(\"11111111-1111-1111-1111-111111111111\"); subject_kind(\"user\"); subject_user_uuid(\"11111111-1111-1111-1111-111111111111\"); session(\"inspect-scoped\"); device(\"registered-device\"); credential_id(\"issued-credential\"); device_pop_key(\"{}\"); expires_at(2000-01-01T00:00:00Z); check if operation(\"ReadContent\"); check if resource(\"spool\", \"private/one\"); check if time($t), $t < 2000-01-01T00:00:00Z;",hex::encode(key.public().to_bytes())).as_str()).expect("expired scoped facts").build(&key).expect("token");
        (token, key.public())
    }
    #[test]
    fn inspection_authenticates_identity_without_granting_unrelated_or_expired_actions() {
        let (token, root) = fixture();
        let inspected =
            inspect_verified_credential(&token, &root).expect("metadata remains inspectable");
        assert_eq!(
            inspected.asserted_account,
            Some(uuid::Uuid::from_bytes([0x11; 16]))
        );
        assert_eq!(inspected.device_id.as_deref(), Some("registered-device"));
        assert_eq!(
            inspected.credential_id.as_deref(),
            Some("issued-credential")
        );
        assert_eq!(inspected.proof_public_key, root.to_bytes());
        assert_eq!(inspected.expires_at_unix_seconds, 946684800);
        assert!(!inspected.revocation_ids.is_empty());
        assert!(
            crate::authorize_at(
                &token,
                "ObserveIdentity",
                chrono::Utc::now(),
                None,
                &[],
                None
            )
            .is_err()
        );
        assert!(
            crate::authorize_at(
                &token,
                "ReadContent",
                chrono::Utc::now(),
                None,
                &[],
                Some(("spool", "private/one"))
            )
            .is_err()
        );
    }
    #[test]
    fn inspection_verifies_effective_delegation_key_instead_of_appended_claims() {
        let (token, root) = fixture();
        let encoded = token.to_base64().expect("encoded");
        let child = SigningKey::from_bytes(&[18; 32]);
        let key = child.verifying_key().to_bytes();
        let signature = SigningKey::from_bytes(&[17; 32])
            .sign(&crate::key_delegation::statement(&encoded, &key).expect("canonical delegation"))
            .to_bytes();
        let delegated = crate::key_delegation::append(
            &encoded,
            &key,
            &signature,
            BlockBuilder::new()
                .code("device(\"appended-device\"); credential_id(\"appended-credential\");")
                .expect("untrusted appended selectors"),
        )
        .expect("delegated");
        let verified = Biscuit::from_base64(&delegated, root).expect("signature chain");
        let inspected =
            inspect_verified_credential(&verified, &root).expect("original authority selectors");
        assert_eq!(inspected.device_id.as_deref(), Some("registered-device"));
        assert_eq!(
            inspected.credential_id.as_deref(),
            Some("issued-credential")
        );
        assert_eq!(
            inspect_verified_credential(&verified, &root)
                .expect("verified child")
                .proof_public_key,
            key
        );
        let forged = token
            .append(
                BlockBuilder::new()
                    .fact(format!("device_pop_key(\"{}\")", hex::encode(key)).as_str())
                    .expect("append unsupported claim"),
            )
            .expect("signature-valid attenuation");
        assert!(
            inspect_verified_credential(&forged, &root).is_err(),
            "unsigned effective-key rewrite must fail"
        );
    }
}
