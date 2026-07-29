use heddle_client::owner_authorization::{
    AuthorizationError, AuthorizationKey, GuardianSigner, PaperRecoveryKit, RecoverySetup,
    confirm_custodial_weft_only, create_human_owner_root, verify_owner_root,
    wire::RecoveryGuardianKind,
};
use prost::Message;

#[test]
fn weft_only_recovery_requires_explicit_custodial_confirmation() {
    let silent = RecoverySetup::with_threshold(
        1,
        vec![GuardianSigner::weft(
            AuthorizationKey::from_seed([10; 32]).expect("Weft guardian"),
        )],
    );
    assert!(matches!(silent, Err(AuthorizationError::Invalid(_))));

    let explicit = RecoverySetup::custodial_weft_only(
        GuardianSigner::weft(AuthorizationKey::from_seed([10; 32]).expect("Weft guardian")),
        confirm_custodial_weft_only(),
    )
    .expect("explicit custodial recovery");
    let authority = AuthorizationKey::from_seed([11; 32]).expect("authority");
    let root = create_human_owner_root([12; 16], &authority, &explicit).expect("explicit root");
    verify_owner_root(&root).expect("explicitly confirmed policy verifies");
}

#[test]
fn guardian_kind_cannot_be_forged_from_weft_to_paper() {
    let authority = AuthorizationKey::from_seed([13; 32]).expect("authority");
    let recovery = RecoverySetup::recommended(vec![
        GuardianSigner::paper(AuthorizationKey::from_seed([14; 32]).expect("paper")),
        GuardianSigner::weft(AuthorizationKey::from_seed([15; 32]).expect("Weft")),
    ])
    .expect("two-factor recovery");
    let mut root = create_human_owner_root([16; 16], &authority, &recovery).expect("signed root");
    verify_owner_root(&root).expect("unmodified guardian provenance verifies");

    let forged = root
        .root
        .as_mut()
        .expect("root")
        .recovery_policy
        .as_mut()
        .expect("policy")
        .guardians
        .iter_mut()
        .find(|guardian| guardian.kind == RecoveryGuardianKind::Weft as i32)
        .expect("Weft guardian");
    forged.kind = RecoveryGuardianKind::Paper as i32;
    assert!(
        verify_owner_root(&root).is_err(),
        "signed guardian provenance must make a WEFT-to-PAPER rewrite fail closed"
    );
}

#[test]
fn paper_kit_round_trips_the_256_bit_seed_but_wires_only_its_public_key() {
    let seed = [17; 32];
    let kit = PaperRecoveryKit::from_seed(seed).expect("paper kit");
    let encoded = kit.to_base64();
    assert_eq!(
        encoded.len(),
        43,
        "unpadded base64 encodes exactly 256 bits"
    );
    let restored = PaperRecoveryKit::from_base64(&encoded).expect("restored kit");
    assert_eq!(
        restored.key().verification_key(),
        kit.key().verification_key()
    );

    let authority = AuthorizationKey::from_seed([18; 32]).expect("authority");
    let recovery = RecoverySetup::recommended(vec![
        GuardianSigner::paper(restored.into_key()),
        GuardianSigner::social(AuthorizationKey::from_seed([19; 32]).expect("social guardian")),
    ])
    .expect("paper-backed recovery");
    let root = create_human_owner_root([20; 16], &authority, &recovery).expect("root");
    let root_bytes = root.encode_to_vec();
    assert!(
        !root_bytes.windows(seed.len()).any(|window| window == seed),
        "the printable paper seed must never enter the wire object"
    );
    verify_owner_root(&root).expect("public paper guardian verifies");
}
