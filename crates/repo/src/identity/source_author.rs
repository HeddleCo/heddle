//! Offline original device authorship. Enrollment retains the device's own
//! sealed proof; captures copy it into signed operations without hosted calls.
use std::{io::Read, path::Path};

use anyhow::{Context, Result, ensure};
use objects::object::{CollaborationActor, thread_replication::SourceAuthor};
use prost::Message;
const MAX_BYTES: usize = 96 * 1024;
#[derive(Clone, PartialEq, Message)]
struct Stored {
    #[prost(uint32, tag = "1")]
    format: u32,
    #[prost(bytes = "vec", tag = "2")]
    publisher: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    account: Vec<u8>,
    #[prost(string, optional, tag = "4")]
    agent: Option<String>,
    #[prost(bytes = "vec", tag = "5")]
    authority: Vec<u8>,
}
/// This signature-verified inspection binds identity but does not grant any RPC
/// rights. Original source admission still evaluates all caveats and revocations.
pub fn publish(
    home: &Path,
    authority: &crate::device_authority::DeviceAuthority,
    mint_root: &[u8; 32],
    publisher: &[u8; 32],
    token: &biscuit_auth::Biscuit,
    now: i64,
) -> Result<()> {
    authority.verify_mint_root(mint_root, now)?;
    authority.verify_publisher(publisher)?;
    let key = biscuit_auth::PublicKey::from_bytes(mint_root, biscuit_auth::Algorithm::Ed25519)?;
    let verified =
        heddle_biscuit_verifier::parse_token(&token.to_base64()?, std::slice::from_ref(&key))?;
    let inspected = heddle_biscuit_verifier::inspect_verified_credential(&verified, &key)?;
    ensure!(
        !inspected
            .revocation_ids
            .iter()
            .any(|id| authority.revoked_ids.contains(id)),
        "original source credential is revoked"
    );
    ensure!(
        inspected.proof_public_key == publisher,
        "original source proof belongs to another signing key"
    );
    let account = uuid::Uuid::parse_str(
        &authority
            .owner
            .owner
            .as_ref()
            .context("account owner required")?
            .id,
    )?;
    ensure!(
        inspected
            .asserted_account
            .is_none_or(|claimed| claimed == account),
        "original source proof names another account"
    );
    let proof = crate::thread_replication::metadata::prepare_control_authority(
        authority, mint_root, token, now,
    )?;
    let stored = Stored {
        format: 1,
        publisher: publisher.to_vec(),
        account: account.as_bytes().to_vec(),
        agent: inspected.agent_id,
        authority: proof,
    };
    ensure!(
        stored.encoded_len() <= MAX_BYTES,
        "source author persistence bound"
    );
    let directory = home.join("state/source-authors");
    objects::fs_atomic::create_private_dir_all(&directory)?;
    objects::fs_atomic::write_file_atomic_secret(
        &directory.join(format!("{}.pb", hex::encode(publisher))),
        &stored.encode_to_vec(),
    )?;
    Ok(())
}
/// Missing enrollment remains explicit local-key authorship. Expired envelopes
/// remain exact historical claims; this loader never renews or authorizes them.
pub fn load(home: &Path, publisher: &[u8; 32], spool: uuid::Uuid) -> Result<SourceAuthor> {
    let path = home
        .join("state/source-authors")
        .join(format!("{}.pb", hex::encode(publisher)));
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SourceAuthor::LocalKey);
        }
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "source author persistence bound");
    let stored = Stored::decode(bytes.as_slice())?;
    ensure!(
        stored.format == 1 && stored.encode_to_vec() == bytes && stored.publisher == publisher,
        "canonical source author belongs to another signer"
    );
    let actor = CollaborationActor {
        principal_id: uuid::Uuid::from_slice(&stored.account)?,
        agent_id: stored.agent,
    };
    Ok(SourceAuthor::account(spool, actor, stored.authority)?)
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer};

    use super::*;
    #[test]
    fn offline_source_claim_preserves_exact_device_author_and_sealed_proof() {
        let home = tempfile::tempdir().expect("home");
        let spool = uuid::Uuid::from_u128(43);
        let signer = Ed25519Signer::from_seed(&[71; 32]).expect("signer");
        let recovery = Ed25519Signer::from_seed(&[72; 32]).expect("recovery");
        let key: [u8; 32] = signer.public_key().try_into().expect("key");
        assert_eq!(
            load(home.path(), &key, spool).expect("unregistered local source"),
            SourceAuthor::LocalKey
        );
        let root = crate::sign_custodial_owner_root(&signer, &recovery, [9; 16], [5; 32])
            .expect("owner root");
        let binding =
            crate::sign_custodial_owner_binding(&signer, &root, [6; 32]).expect("binding");
        let verified = heddleco_capability_verifier::verify_owner_root(&root).expect("owner");
        let owner = api::heddle::api::v2alpha1::OwnerState {
            owner: Some(api::heddle::api::v2alpha1::PrincipalRef {
                id: uuid::Uuid::from_bytes([9; 16]).to_string(),
            }),
            root: Some(root),
            binding: Some(binding),
            version: verified.state_hash().to_vec(),
            ..Default::default()
        };
        let authority = crate::device_authority::DeviceAuthority {
            owner,
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        };
        let pair = biscuit_auth::KeyPair::from(
            &biscuit_auth::PrivateKey::from_bytes(&[71; 32], biscuit_auth::Algorithm::Ed25519)
                .expect("root"),
        );
        let account = uuid::Uuid::from_bytes([9; 16]);
        let token=biscuit_auth::Biscuit::builder().code(format!("user(\"{account}\"); subject_kind(\"user\"); subject_user_uuid(\"{account}\"); session(\"source-author\"); device_pop_key(\"{}\"); expires_at(2100-01-01T00:00:00Z); check if operation(\"PublishContent\");",hex::encode(key))).expect("facts").build(&pair).expect("signed token");
        publish(home.path(), &authority, &key, &key, &token, 100)
            .expect("retain original device authority");
        let original = load(home.path(), &key, spool).expect("offline original");
        let SourceAuthor::Account {
            actor,
            authority: envelope,
            ..
        } = &original
        else {
            panic!("account author")
        };
        assert_eq!(actor.principal_id, account);
        assert_eq!(actor.agent_id, None);
        let proof = api::heddle::api::v2alpha1::ThreadControlAuthority::decode(envelope.as_slice())
            .expect("portable envelope");
        let sealed = biscuit_auth::Biscuit::from(&proof.sealed_biscuit, pair.public())
            .expect("sealed signature");
        assert!(
            matches!(
                sealed.seal(),
                Err(biscuit_auth::error::Token::AlreadySealed)
            ),
            "stored proof never includes appendable credential secret"
        );
        assert_eq!(
            load(home.path(), &key, spool).expect("offline replay"),
            original,
            "reading cannot remint or rewrite author claim"
        );
        let other: [u8; 32] = Ed25519Signer::from_seed(&[73; 32])
            .expect("other")
            .public_key()
            .try_into()
            .expect("key");
        assert!(
            publish(home.path(), &authority, &key, &other, &token, 100)
                .expect_err("publisher mismatch")
                .to_string()
                .contains("another signing key"),
            "browser capability cannot be installed for a device signer"
        );
        assert_eq!(
            load(home.path(), &other, spool).expect("other stays local"),
            SourceAuthor::LocalKey
        );
    }
}
