//! The sole Biscuit signature-version boundary. Datalog block versions are unrelated.
#![allow(clippy::disallowed_methods)]

use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use biscuit_auth::{
    Biscuit, KeyPair, RootKeyProvider, UnverifiedBiscuit, builder::BiscuitBuilder, format::schema,
};
use prost010::Message;

use crate::BiscuitError;

fn invalid(error: impl std::fmt::Display) -> BiscuitError {
    BiscuitError::Invalid(error.to_string())
}

fn require_wire_v1(bytes: &[u8]) -> Result<schema::Biscuit, BiscuitError> {
    let wire = schema::Biscuit::decode(bytes).map_err(invalid)?;
    if wire.authority.version != Some(1) {
        return Err(BiscuitError::Invalid(
            "Biscuit authority block must use signature-v1".into(),
        ));
    }
    for (index, block) in wire.blocks.iter().enumerate() {
        if block.version != Some(1) {
            return Err(BiscuitError::Invalid(format!(
                "Biscuit block {} must use signature-v1",
                index + 1
            )));
        }
    }
    Ok(wire)
}

/// Reject any token with a signature-v0 authority or appended block.
pub fn require_v1(token: &Biscuit) -> Result<(), BiscuitError> {
    if token.container().authority.version != 1 {
        return Err(BiscuitError::Invalid(
            "Biscuit authority block must use signature-v1".into(),
        ));
    }
    for (index, block) in token.container().blocks.iter().enumerate() {
        if block.version != 1 {
            return Err(BiscuitError::Invalid(format!(
                "Biscuit block {} must use signature-v1",
                index + 1
            )));
        }
    }
    Ok(())
}

/// Parse a token only after checking every signed block's signature version.
pub fn parse_unverified(bytes: &[u8]) -> Result<UnverifiedBiscuit, BiscuitError> {
    require_wire_v1(bytes)?;
    UnverifiedBiscuit::from(bytes).map_err(invalid)
}

/// Decode a URL-safe bearer and enforce signature-v1 before parsing it.
pub fn parse_unverified_base64(token: impl AsRef<[u8]>) -> Result<UnverifiedBiscuit, BiscuitError> {
    let bytes = URL_SAFE.decode(token).map_err(invalid)?;
    parse_unverified(&bytes)
}

/// Verify a binary token against its root key and require signature-v1 throughout.
pub fn verify<KP: RootKeyProvider>(
    bytes: &[u8],
    key_provider: KP,
) -> Result<Biscuit, BiscuitError> {
    require_wire_v1(bytes)?;
    let token = Biscuit::from(bytes, key_provider).map_err(invalid)?;
    require_v1(&token)?;
    Ok(token)
}

/// Verify a URL-safe bearer against its root key and require signature-v1 throughout.
pub fn verify_base64<KP: RootKeyProvider>(
    token: impl AsRef<[u8]>,
    key_provider: KP,
) -> Result<Biscuit, BiscuitError> {
    let bytes = URL_SAFE.decode(token).map_err(invalid)?;
    verify(&bytes, key_provider)
}

fn resign_authority(token: Biscuit, root: &KeyPair) -> Result<Biscuit, BiscuitError> {
    let mut wire =
        schema::Biscuit::decode(token.to_vec().map_err(invalid)?.as_slice()).map_err(invalid)?;
    if !wire.blocks.is_empty() {
        return Err(BiscuitError::Internal(
            "newly built Biscuit root contains appended blocks".into(),
        ));
    }
    let authority = &mut wire.authority;
    let mut payload = b"\0BLOCK\0\0VERSION\0".to_vec();
    payload.extend_from_slice(&1_u32.to_le_bytes());
    payload.extend_from_slice(b"\0PAYLOAD\0");
    payload.extend_from_slice(&authority.block);
    payload.extend_from_slice(b"\0ALGORITHM\0");
    payload.extend_from_slice(&authority.next_key.algorithm.to_le_bytes());
    payload.extend_from_slice(b"\0NEXTKEY\0");
    payload.extend_from_slice(&authority.next_key.key);
    authority.signature = root.sign(&payload).map_err(invalid)?.to_bytes().to_vec();
    authority.version = Some(1);
    let bytes = wire.encode_to_vec();
    verify(&bytes, root.public())
}

/// Build a root with a signature-v1 authority and its original proof key.
pub fn build_root(builder: BiscuitBuilder, root: &KeyPair) -> Result<Biscuit, BiscuitError> {
    let token = builder.build(root).map_err(invalid)?;
    resign_authority(token, root)
}

/// Build a deterministic signature-v1 root for cross-language fixtures.
pub fn build_root_with_key_pair(
    builder: BiscuitBuilder,
    root: &KeyPair,
    next: &KeyPair,
) -> Result<Biscuit, BiscuitError> {
    let token = builder
        .build_with_key_pair(root, Default::default(), next)
        .map_err(invalid)?;
    resign_authority(token, root)
}

/// Builder methods that preserve the signature-v1 root invariant in fixtures.
pub trait BiscuitBuilderV1Ext {
    /// Build a root with a v1 authority block.
    fn build_v1(self, root: &KeyPair) -> Result<Biscuit, BiscuitError>;

    /// Build a deterministic root with a v1 authority block.
    fn build_v1_with_key_pair(
        self,
        root: &KeyPair,
        next: &KeyPair,
    ) -> Result<Biscuit, BiscuitError>;
}

impl BiscuitBuilderV1Ext for BiscuitBuilder {
    fn build_v1(self, root: &KeyPair) -> Result<Biscuit, BiscuitError> {
        build_root(self, root)
    }

    fn build_v1_with_key_pair(
        self,
        root: &KeyPair,
        next: &KeyPair,
    ) -> Result<Biscuit, BiscuitError> {
        build_root_with_key_pair(self, root, next)
    }
}

#[cfg(test)]
mod tests {
    use biscuit_auth::{Algorithm, PrivateKey, builder::BlockBuilder};

    use super::*;

    fn key(seed: u8) -> KeyPair {
        let private =
            PrivateKey::from_bytes(&[seed; 32], Algorithm::Ed25519).expect("test private key");
        KeyPair::from(&private)
    }

    fn legacy_v0_fixture(root: &KeyPair) -> Biscuit {
        Biscuit::builder()
            .fact("fixture(true)")
            .expect("fact")
            .build(root)
            .expect("v0 control")
    }

    #[test]
    fn parse_token_rejects_v0_authority() {
        let root = key(1);
        let legacy = legacy_v0_fixture(&root);
        assert_eq!(legacy.container().authority.version, 0);
        let token = legacy.to_base64().expect("base64");
        assert!(Biscuit::from_base64(&token, root.public()).is_ok());
        let error = crate::parse_token(&token, &[root.public()]).expect_err("v0 rejected");
        assert!(
            error
                .to_string()
                .contains("authority block must use signature-v1")
        );
        assert!(parse_unverified_base64(&token).is_err());
    }

    #[test]
    fn authorize_at_rejects_v0_direct() {
        let root = key(1);
        let legacy = legacy_v0_fixture(&root);
        let error =
            crate::authorize_at(&legacy, "ReadContent", chrono::Utc::now(), None, &[], None)
                .expect_err("direct authorization rejects v0");
        assert!(error.to_string().contains("signature-v1"), "{error}");
    }

    #[test]
    fn key_delegation_append_refuses_v0_parent() {
        let root = key(1);
        let parent = legacy_v0_fixture(&root).to_base64().expect("v0 bearer");
        let error = crate::key_delegation::append(&parent, &[3; 32], &[4; 64], BlockBuilder::new())
            .expect_err("v0 parent refused");
        assert!(error.to_string().contains("signature-v1"), "{error}");
    }

    #[test]
    fn appends_to_v1_root_stay_v1() {
        let root = key(2);
        let token = build_root(Biscuit::builder(), &root).expect("v1 root");
        let appended = token
            .append(BlockBuilder::new().fact("child(true)").expect("fact"))
            .expect("verified append");
        require_v1(&appended).expect("verified append remains v1");
        let keyed = token
            .append_with_keypair(
                &key(11),
                BlockBuilder::new().fact("keyed(true)").expect("fact"),
            )
            .expect("verified keyed append");
        require_v1(&keyed).expect("verified keyed append remains v1");
        let unverified = parse_unverified(&token.to_vec().expect("root bytes"))
            .expect("v1 unverified")
            .append(BlockBuilder::new().fact("child(true)").expect("fact"))
            .expect("unverified append");
        let bytes = unverified.to_vec().expect("child bytes");
        let verified = verify(&bytes, root.public()).expect("unverified append remains v1");
        assert_eq!(verified.container().blocks[0].version, 1);
        let parent = token.to_base64().expect("parent bearer");
        let child = crate::key_delegation::append(&parent, &[3; 32], &[4; 64], BlockBuilder::new())
            .expect("proof-key append");
        let delegated = verify_base64(child, root.public()).expect("delegation stays v1");
        assert_eq!(delegated.container().blocks[0].version, 1);

        let external = key(3);
        let external_block = token
            .third_party_request()
            .expect("request")
            .create_block(
                &external.private(),
                BlockBuilder::new().fact("external(true)").expect("fact"),
            )
            .expect("external block");
        let third_party = token
            .append_third_party_with_keypair(external.public(), external_block, key(4))
            .expect("third-party append");
        require_v1(&third_party).expect("third-party append stays v1");
    }

    #[test]
    fn mixed_chain_rejected_in_both_directions() {
        let root = key(5);
        let legacy = legacy_v0_fixture(&root);
        let external = key(6);
        let external_block = legacy
            .third_party_request()
            .expect("request")
            .create_block(
                &external.private(),
                BlockBuilder::new().fact("external(true)").expect("fact"),
            )
            .expect("external block");
        let mixed = legacy
            .append_third_party_with_keypair(external.public(), external_block, key(7))
            .expect("v1 third-party block on v0 root");
        assert_eq!(mixed.container().authority.version, 0);
        assert_eq!(mixed.container().blocks[0].version, 1);
        assert!(verify(&mixed.to_vec().expect("mixed bytes"), root.public()).is_err());

        let v1 = build_root(Biscuit::builder(), &root).expect("v1 root");
        let appended = v1
            .append(BlockBuilder::new().fact("child(true)").expect("fact"))
            .expect("append");
        let mut wire =
            schema::Biscuit::decode(appended.to_vec().expect("bytes").as_slice()).expect("wire");
        wire.blocks[0].version = None;
        let error = parse_unverified(&wire.encode_to_vec()).expect_err("v0 tail rejected");
        assert!(error.to_string().contains("block 1 must use signature-v1"));
    }

    #[test]
    fn v1_graft_fails() {
        let root = key(8);
        let proof = key(9);
        let next = key(10);
        let a = build_root_with_key_pair(
            Biscuit::builder().fact("prefix(\"a\")").expect("a"),
            &root,
            &proof,
        )
        .expect("a root");
        let b = build_root_with_key_pair(
            Biscuit::builder().fact("prefix(\"b\")").expect("b"),
            &root,
            &proof,
        )
        .expect("b root");
        let a_child = a
            .append_with_keypair(&next, BlockBuilder::new().fact("tail(true)").expect("tail"))
            .expect("a child");
        let mut graft =
            schema::Biscuit::decode(a_child.to_vec().expect("bytes").as_slice()).expect("wire");
        let b_wire =
            schema::Biscuit::decode(b.to_vec().expect("bytes").as_slice()).expect("b wire");
        graft.authority = b_wire.authority;
        assert!(verify(&graft.encode_to_vec(), root.public()).is_err());
    }

    #[test]
    fn api258_reviewer_collision_rejected() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/timeline-origin-collision-v0.json"
        ))
        .expect("API #258 collision fixture");
        let root = key(1);
        for field in ["a_chain_hex", "b_chain_hex"] {
            let bytes =
                hex::decode(fixture[field].as_str().expect("chain hex")).expect("chain bytes");
            assert!(
                Biscuit::from(&bytes, root.public()).is_ok(),
                "{field} v0 control"
            );
            let error = verify(&bytes, root.public()).expect_err("v0 collision chain rejected");
            assert!(error.to_string().contains("signature-v1"), "{error}");
        }
    }
}
