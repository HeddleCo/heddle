//! Portable construction of a child proof-key block. This never issues authority:
//! callers sign with the parent's effective key and hosts verify the complete
//! resulting Biscuit chain, including every ancestor restriction, for each call.
use biscuit_auth::{UnverifiedBiscuit, builder::BlockBuilder};
use chrono::{DateTime, Utc};

use crate::{BiscuitError, BiscuitResultExt as _};

/// Exact bytes signed by the parent's effective proof key. The parent reference
/// is the last revocation identifier, not a third-party external signature.
pub fn statement(parent: &str, child_public_key: &[u8; 32]) -> Result<Vec<u8>, BiscuitError> {
    let token = parse(parent)?;
    Ok(crate::pop_delegation_payload(
        &last_id(&token)?,
        child_public_key,
    ))
}

/// Append an externally signed key transition to an existing bearer. Signature
/// verification belongs to the complete host verifier; parsing/appending alone
/// does not authenticate a token or prove its parent key is trusted.
pub fn append(
    parent: &str,
    child_public_key: &[u8; 32],
    signature: &[u8; 64],
    restrictions: BlockBuilder,
) -> Result<String, BiscuitError> {
    if restrictions
        .facts
        .iter()
        .any(|fact| fact.predicate.name == "pop_delegation")
    {
        return Err(BiscuitError::Invalid(
            "restrictions already contain a key delegation".into(),
        ));
    }
    let token = parse(parent)?;
    let block = restrictions
        .fact(
            format!(
                "pop_delegation(\"{}\", \"{}\", \"{}\")",
                hex::encode(last_id(&token)?),
                hex::encode(child_public_key),
                hex::encode(signature)
            )
            .as_str(),
        )
        .internal_ctx("append child proof-key fact")?;
    token
        .append(block)
        .internal_ctx("append child proof-key block")?
        .to_base64()
        .internal_ctx("encode delegated bearer")
}

/// Require an actual signed-block descendant of the exact current bearer.
/// Account/root equality alone is insufficient: a parallel root token could
/// discard the current bearer’s restrictions. Hosts still verify both tokens.
pub fn require_descendant(parent: &str, child: &str) -> Result<(), BiscuitError> {
    let parent = parse(parent)?;
    let child = parse(child)?;
    let parent_ids = parent.revocation_identifiers();
    let child_ids = child.revocation_identifiers();
    if child_ids.len() <= parent_ids.len() || !child_ids.starts_with(&parent_ids) {
        return Err(BiscuitError::Invalid(
            "delegated credential does not extend the exact parent bearer".into(),
        ));
    }
    Ok(())
}

/// Device delegation preserves the parent's permissions and agent attribution.
/// Only its expiry is narrowed; this adds no independent root or agent identity.
pub fn device_restrictions(expires_at: DateTime<Utc>) -> Result<BlockBuilder, BiscuitError> {
    BlockBuilder::new()
        .check(format!("check if time($now), $now < {}", expires_at.to_rfc3339()).as_str())
        .internal_ctx("device delegation expiry")
}

fn parse(parent: &str) -> Result<UnverifiedBiscuit, BiscuitError> {
    if parent.is_empty() || parent.len() > 96 * 1024 {
        return Err(BiscuitError::Invalid(
            "parent credential is empty or oversized".into(),
        ));
    }
    UnverifiedBiscuit::from_base64(parent.as_bytes())
        .map_err(|error| BiscuitError::Invalid(error.to_string()))
}
fn last_id(token: &UnverifiedBiscuit) -> Result<Vec<u8>, BiscuitError> {
    token
        .revocation_identifiers()
        .last()
        .map(|id| id.to_vec())
        .ok_or_else(|| {
            BiscuitError::Invalid("parent credential has no revocation identifier".into())
        })
}

#[cfg(test)]
mod tests {
    use biscuit_auth::{Algorithm, Biscuit, KeyPair, PrivateKey};
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    fn key(seed: u8) -> KeyPair {
        KeyPair::from(
            &PrivateKey::from_bytes(&[seed; 32], Algorithm::Ed25519).expect("fixture private key"),
        )
    }
    fn fixture() -> (String, SigningKey, SigningKey, DateTime<Utc>) {
        let root = key(11);
        let signer = SigningKey::from_bytes(&[11; 32]);
        let child = SigningKey::from_bytes(&[12; 32]);
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture timestamp");
        let token = Biscuit::builder().code(format!(
            "user(\"00000000-0000-0000-0000-000000000011\"); session(\"fixture-session\"); device_pop_key(\"{}\"); root_established(true); right(\"spool\", \"org/project\", \"admin\"); check if time($now), $now < {};",
            hex::encode(signer.verifying_key().as_bytes()),(now + chrono::Duration::hours(1)).to_rfc3339()
        ).as_str()).expect("authority facts").build_with_key_pair(&root, Default::default(), &key(13)).expect("deterministic parent").to_base64().expect("parent encoding");
        (token, signer, child, now)
    }
    #[test]
    fn device_delegation_retains_parent_admin_and_requires_actual_parent_signature() {
        let (parent, signer, child, now) = fixture();
        let payload = statement(&parent, &child.verifying_key().to_bytes()).expect("statement");
        let signature = signer.sign(&payload).to_bytes();
        let delegated = append(
            &parent,
            &child.verifying_key().to_bytes(),
            &signature,
            device_restrictions(now + chrono::Duration::minutes(5)).expect("expiry"),
        )
        .expect("append");
        require_descendant(&parent, &delegated).expect("actual delegated chain");
        assert!(
            require_descendant(&parent, &parent).is_err(),
            "parent itself is not a child"
        );
        assert!(
            require_descendant(&delegated, &parent).is_err(),
            "cannot discard attenuation"
        );
        let facts = crate::verify_any_at_with_resource(
            &delegated,
            None,
            &[key(11).public()],
            &[],
            "ObserveIdentity",
            None,
            now,
        )
        .expect("verified child");
        assert_eq!(
            facts.cnf,
            Some(hex::encode(child.verifying_key().as_bytes()))
        );
        assert!(facts.rights.iter().any(|right| right.action == "admin"));
        let wrong = SigningKey::from_bytes(&[14; 32]).sign(&payload).to_bytes();
        let rejected = append(
            &parent,
            &child.verifying_key().to_bytes(),
            &wrong,
            device_restrictions(now + chrono::Duration::minutes(5)).expect("expiry"),
        )
        .expect("append untrusted proof");
        assert!(
            crate::verify_any_at_with_resource(
                &rejected,
                None,
                &[key(11).public()],
                &[],
                "ObserveIdentity",
                None,
                now
            )
            .is_err()
        );
        assert!(
            crate::verify_any_at_with_resource(
                &delegated,
                None,
                &[key(11).public()],
                &[],
                "ObserveIdentity",
                None,
                now + chrono::Duration::minutes(6)
            )
            .is_err()
        );
    }
    #[test]
    fn device_delegation_browser_statement_fixture() {
        let (parent, signer, child, now) = fixture();
        let payload = statement(&parent, &child.verifying_key().to_bytes()).expect("statement");
        let signature = signer.sign(&payload).to_bytes();
        let vector = format!(
            "parent={}\nparent_public_key={}\nchild_public_key={}\nstatement={}\nsignature={}\nexpires_at={}\n",
            parent,
            hex::encode(signer.verifying_key().as_bytes()),
            hex::encode(child.verifying_key().as_bytes()),
            hex::encode(payload),
            hex::encode(signature),
            (now + chrono::Duration::minutes(5)).to_rfc3339()
        );
        assert_eq!(
            vector,
            include_str!("../tests/fixtures/device_delegation_v1.txt")
        );
    }
}
