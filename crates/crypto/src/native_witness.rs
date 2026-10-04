//! Job-free native first admission. Import authority cannot enter this arm.
use api::{
    heddle::api::v1alpha2 as wire,
    hybrid_codec::{self, Reject},
    native_witness as contract,
};
use heddleco_capability_verifier::import_delegation::Selection;

use crate::{
    Signer,
    import_authority::{self, NativeAuthorityContext, NativeClosure, Result, WitnessEvidence},
    thread_operation::SignedGenesis,
};

/// Freeze the original and envelope before signing; request PoP follows this.
pub fn sign_genesis_authority(
    original: &wire::SignedRecord,
    envelope: &[u8],
    selection: &Selection<'_>,
    now_seconds: i64,
    signer: &(impl Signer + ?Sized),
) -> Result<wire::SignedNativeGenesisAuthorityV1> {
    let (_, genesis) = import_authority::verify_native_genesis(original)?;
    if signer.public_key() != genesis.creator {
        return Err(Reject::KeyRole.into());
    }
    let (identity, chain, _) =
        heddleco_capability_verifier::import_delegation::native_identity(selection, now_seconds)?;
    let mut signatures = 1u32.to_be_bytes().to_vec();
    signatures.extend(hybrid_codec::canonical(&original.signatures[0])?);
    let body = wire::NativeGenesisAuthorityV1 {
        format_version: 1,
        identity: Some(identity),
        owner_kind: match genesis.owner {
            heddle_object_model::object::thread_replication::GenesisOwner::Account(_) => 1,
            heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(_) => 2,
        },
        genesis_digest: genesis.id()?.as_bytes().to_vec(),
        original_signatures_digest: hybrid_codec::hash(&[
            b"heddle-hosted-original-signatures-v1",
            &signatures,
        ]),
        creator_public_key: genesis.creator.to_vec(),
        creator_authority_envelope_digest: hybrid_codec::hash(&[envelope]),
        owner_chain_digest: chain,
        publisher_key_id: hybrid_codec::key_id(&genesis.creator),
    };
    let signed = wire::SignedNativeGenesisAuthorityV1 {
        creator_signature: Some(wire::AuthorizationSignature {
            signer_key_id: body.publisher_key_id.clone(),
            signature: signer.sign(&hybrid_codec::signing_digest(
                contract::GENESIS_DOMAIN,
                &body,
            )?)?,
        }),
        body: Some(body),
    };
    contract::verify_genesis_authority(&signed, original, envelope)?;
    Ok(signed)
}

/// Check the complete exact native binding, current boundary selection or
/// original StartThread authority, and the independently resolved witness.
pub fn verify_genesis_payload(
    payload: &wire::NativeGenesisWitnessV1,
    evidence: &WitnessEvidence,
    selection: &Selection<'_>,
    closure: &NativeClosure,
    context: &NativeAuthorityContext<'_>,
    revoked: impl Fn(heddleco_capability_verifier::thread_control_authority::Revocation<'_>) -> bool,
) -> Result<SignedGenesis> {
    let statement = evidence.signed().body.as_ref().ok_or(Reject::Canonical)?;
    contract::verify_genesis_payload(statement, payload)?;
    let original = payload.original_genesis.as_ref().ok_or(Reject::Canonical)?;
    let (signed, genesis) = import_authority::verify_native_genesis(original)?;
    if context
        .forbidden_authority_keys
        .iter()
        .any(|k| k.as_slice() == genesis.creator)
        || context
            .known_job_associations
            .iter()
            .any(|(k, _)| k.as_slice() == genesis.creator)
    {
        return Err(Reject::KeyRole.into());
    }
    let binding = payload.binding.as_ref().ok_or(Reject::GenesisBinding)?;
    heddleco_capability_verifier::native_genesis::verify_binding(
        binding,
        original,
        &payload.creator_authority_envelope,
        selection,
    )?;
    if statement.basis == 2 {
        import_authority::verify_native_genesis_boundary(
            payload, evidence, closure, context, &revoked,
        )?;
    } else {
        heddleco_capability_verifier::native_genesis::verify_original_authority(
            binding,
            original,
            &payload.creator_authority_envelope,
            selection,
            statement.observed_at_unix_millis / 1000,
            revoked,
        )?;
    }
    if closure.genesis(&genesis.id()?)? != &genesis {
        return Err(Reject::Scope.into());
    }
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn creator_signs_the_frozen_native_genesis_and_exact_authority() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/native-host-witness-v1.json"
        ))
        .expect("published vectors");
        for name in ["start_thread", "local_adopt_push"] {
            let bundle = wire::NativePublicProofBundleV1::decode(
                hex::decode(
                    fixture["wire_vectors"][name]["wire_hex"]
                        .as_str()
                        .expect("wire"),
                )
                .expect("hex")
                .as_slice(),
            )
            .expect("bundle");
            let h = &bundle.owner_histories[0];
            let owner =
                heddleco_capability_verifier::verify_owner_root(h.root.as_ref().expect("root"))
                    .expect("owner");
            let initial = owner.owner_id();
            let signed_genesis = bundle.owner_genesis.as_ref().expect("Spool");
            let digest = heddleco_capability_verifier::creation::spool_genesis_digest(
                signed_genesis.genesis.as_ref().expect("body"),
            )
            .expect("digest");
            let limits =
                heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
            let keyring = heddleco_capability_verifier::verify_clone_keyring(
                wire::CloneAuthorizationKeyring {
                    format_version: 1,
                    spool_uuid: signed_genesis
                        .genesis
                        .as_ref()
                        .expect("body")
                        .spool_uuid
                        .clone(),
                    canonical_spool_path_segments: vec!["acme".into(), "imports".into()],
                    owner_genesis: Some(signed_genesis.clone()),
                    owner_root: h.root.clone(),
                    accepted_transitions: h.accepted_transitions.clone(),
                    accepted_state_hash: h.state_hash.clone(),
                    pin: Some(wire::CloneOwnerPin {
                        kind: 2,
                        expected_owner_id: initial.to_vec(),
                        first_seen_unix_seconds: 1000,
                    }),
                    ..Default::default()
                },
                1000,
                limits,
                &[],
            )
            .expect("independent lineage");
            let selection = Selection {
                owner: &owner,
                keyring: &keyring,
                spool_genesis_digest: &digest,
                initial_owner_id: &initial,
                limits,
            };
            let payload = &bundle.genesis_witnesses[0];
            let original = payload.original_genesis.as_ref().expect("original");
            let creator_key = &payload
                .binding
                .as_ref()
                .expect("binding")
                .body
                .as_ref()
                .expect("body")
                .creator_public_key;
            let seed = fixture["keys"]
                .as_object()
                .expect("keys")
                .values()
                .find(|k| {
                    hex::decode(k["public_key_hex"].as_str().expect("public key")).expect("key")
                        == *creator_key
                })
                .expect("original creator seed");
            let creator = crate::Ed25519Signer::from_seed(
                &hex::decode(seed["seed_hex"].as_str().expect("seed")).expect("seed hex"),
            )
            .expect("original creator");
            let signed = sign_genesis_authority(
                original,
                &payload.creator_authority_envelope,
                &selection,
                1000,
                &creator,
            )
            .expect("creator binding");
            assert_eq!(
                Some(signed.clone()),
                payload.binding,
                "{name}: published signing contract"
            );
            heddleco_capability_verifier::native_genesis::verify_original_authority(
                &signed,
                original,
                &payload.creator_authority_envelope,
                &selection,
                1000,
                |_| false,
            )
            .expect("native authority control");
            let courier = crate::Ed25519Signer::from_seed(&[3; 32]).expect("courier");
            assert!(
                sign_genesis_authority(
                    original,
                    &payload.creator_authority_envelope,
                    &selection,
                    1000,
                    &courier
                )
                .is_err()
            );
            assert!(
                contract::verify_genesis_authority(&signed, original, b"swapped envelope").is_err()
            );
            contract::verify_genesis_authority(
                &signed,
                original,
                &payload.creator_authority_envelope,
            )
            .expect("nearby exact-envelope control");
        }
    }
}
