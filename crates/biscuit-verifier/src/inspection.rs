//! Signature-verified metadata extraction without implying operation authority.
use biscuit_auth::{Biscuit, PublicKey};

use crate::{BiscuitError, BiscuitFacts, authorizer_limits};

/// A selector retains its storage namespace; device IDs and proof keys are
/// not interchangeable with session, credential, or signed-block IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevocationSelector<'a> {
    Session(&'a str),
    Credential(&'a str),
    Block(&'a str),
    Device(&'a str),
    EnvelopeDeviceKey(&'a str),
}
impl<'a> RevocationSelector<'a> {
    pub fn value(self) -> &'a str {
        match self {
            Self::Session(value)
            | Self::Credential(value)
            | Self::Block(value)
            | Self::Device(value)
            | Self::EnvelopeDeviceKey(value) => value,
        }
    }
}
pub(crate) fn credential_selectors<'a>(
    session: &'a str,
    credential: Option<&'a str>,
    blocks: &'a [String],
    device: Option<&'a str>,
    envelope_key: Option<&'a str>,
) -> impl Iterator<Item = RevocationSelector<'a>> {
    std::iter::once(RevocationSelector::Session(session))
        .chain(credential.map(RevocationSelector::Credential))
        .chain(
            blocks
                .iter()
                .map(|value| RevocationSelector::Block(value.as_str())),
        )
        .chain(device.map(RevocationSelector::Device))
        .chain(envelope_key.map(RevocationSelector::EnvelopeDeviceKey))
        .filter(|selector| !selector.value().is_empty())
}
/// Owned observations moved from an inspected credential, with no duplicated
/// flat list. These selectors report provenance; they grant no authority.
#[derive(Debug)]
pub struct CredentialRevocations {
    pub session_id: String,
    pub credential_id: Option<String>,
    pub block_ids: Vec<String>,
    pub device_id: Option<String>,
    pub envelope_device_pubkey_hex: Option<String>,
}
impl CredentialRevocations {
    pub fn selectors(&self) -> impl Iterator<Item = RevocationSelector<'_>> {
        credential_selectors(
            &self.session_id,
            self.credential_id.as_deref(),
            &self.block_ids,
            self.device_id.as_deref(),
            self.envelope_device_pubkey_hex.as_deref(),
        )
    }
    pub fn identifiers(&self) -> impl Iterator<Item = &str> {
        self.selectors().filter_map(|selector| match selector {
            RevocationSelector::Session(value)
            | RevocationSelector::Credential(value)
            | RevocationSelector::Block(value) => Some(value),
            _ => None,
        })
    }
}

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
    /// Root key supplied by the signature-verifying caller, never a token fact.
    pub envelope_device_pubkey_hex: Option<String>,
}
impl InspectedCredential {
    pub fn revocation_selectors(&self) -> impl Iterator<Item = RevocationSelector<'_>> {
        credential_selectors(
            &self.session_id,
            self.credential_id.as_deref(),
            &self.revocation_ids,
            self.device_id.as_deref(),
            self.envelope_device_pubkey_hex.as_deref(),
        )
    }
    pub fn into_revocations(self) -> CredentialRevocations {
        CredentialRevocations {
            session_id: self.session_id,
            credential_id: self.credential_id,
            block_ids: self.revocation_ids,
            device_id: self.device_id,
            envelope_device_pubkey_hex: self.envelope_device_pubkey_hex,
        }
    }
    /// Session, credential, and block IDs; typed selectors also include devices.
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
        envelope_device_pubkey_hex: Some(hex::encode(root.to_bytes())),
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
        let root_hex = hex::encode(root.to_bytes());
        let selectors = inspected.revocation_selectors().collect::<Vec<_>>();
        assert!(selectors.contains(&RevocationSelector::Device("registered-device")));
        assert!(
            selectors.contains(&RevocationSelector::EnvelopeDeviceKey(&root_hex)),
            "inspection must retain actual verified root selector"
        );
        assert!(selectors.contains(&RevocationSelector::Session("inspect-scoped")));
        assert!(selectors.contains(&RevocationSelector::Credential("issued-credential")));
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
