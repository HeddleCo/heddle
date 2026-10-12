// SPDX-License-Identifier: Apache-2.0
use super::*;
use crypto::{
    AEAD_AES256_GCM_V1, AeadCiphertext, Dek, SoftwareRecipientSecret, WrappedDek, decrypt_padded,
    unwrap_dek,
};

#[test]
fn rustcrypto_at_rest_golden_vectors() {
    let sealed = AeadCiphertext {
        alg: AEAD_AES256_GCM_V1,
        nonce: [0x22; 12],
        pad_bucket: 64,
        ciphertext: include_bytes!("../../crypto/tests/fixtures/padded_ciphertext-v1.bin").to_vec(),
    };
    let (_, encoded) = StoredCiphertext::from_aead(&sealed).expect("encode old ciphertext");
    let old_encoded = include_bytes!("../tests/fixtures/ciphertext-v1.msgpack");
    assert_eq!(
        encoded, old_encoded,
        "stored ciphertext bytes must not change"
    );
    let wrapped = crate::types::WrappedDekRecord {
        recipient_id: RecipientId::from_bytes([0x55; 32]),
        ephemeral_public: include_bytes!("../../crypto/tests/fixtures/ephemeral_public-v1.bin")
            .to_vec(),
        nonce: vec![0x22; 12],
        ciphertext: include_bytes!("../../crypto/tests/fixtures/wrapped_dek-v1.bin").to_vec(),
    };
    let encoded_wrap = encode_named(&wrapped, "wrap").expect("encode old wrap");
    let old_encoded_wrap = include_bytes!("../tests/fixtures/wrapped-dek-v1.msgpack");
    assert_eq!(
        encoded_wrap, old_encoded_wrap,
        "stored wrap bytes must not change"
    );
    let decoded = decode_ciphertext(old_encoded).expect("read old stored format");
    let decoded_wrap: crate::types::WrappedDekRecord =
        rmp_serde::from_slice(old_encoded_wrap).expect("read old stored wrap");
    let recipient = SoftwareRecipientSecret::from_bytes([0x31; 32]);
    let dek = unwrap_dek(
        &WrappedDek {
            ephemeral_public: decoded_wrap.ephemeral_public.try_into().expect("public"),
            nonce: decoded_wrap.nonce.try_into().expect("nonce"),
            ciphertext: decoded_wrap.ciphertext,
        },
        &recipient,
        b"heddle-golden-aad-v1\0slot=TOKEN",
    )
    .expect("open persisted old wrap");
    assert_eq!(dek.as_bytes(), Dek::from_bytes([0x11; 32]).as_bytes());
    let old = AeadCiphertext {
        alg: AEAD_AES256_GCM_V1,
        nonce: decoded.nonce.try_into().expect("nonce"),
        ciphertext: decoded.ciphertext,
        pad_bucket: decoded.pad_bucket,
    };
    assert_eq!(
        decrypt_padded(&dek, &old, b"heddle-golden-aad-v1\0slot=TOKEN")
            .expect("open persisted old ciphertext"),
        b"old ciphertext must still open\0\xff"
    );
}
