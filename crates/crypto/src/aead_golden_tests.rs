// SPDX-License-Identifier: Apache-2.0
// Captured before the RustCrypto upgrade. Fixed secrets are test data only.
use super::*;

#[test]
fn rustcrypto_golden_vectors() {
    let key = [0x11; 32];
    let nonce = [0x22; 12];
    let aad = b"heddle-golden-aad-v1\0slot=TOKEN";
    let plaintext = b"old ciphertext must still open\0\xff";
    let cipher = Aes256Gcm::new((&key).into());
    let raw = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("encrypt");
    let recipient = SoftwareRecipientSecret::from_bytes([0x31; 32]);
    let ephemeral = SoftwareRecipientSecret::from_bytes([0x42; 32]);
    let shared = ephemeral
        .0
        .diffie_hellman(&PublicKey::from(recipient.public_key()));
    let reverse = recipient
        .0
        .diffie_hellman(&PublicKey::from(ephemeral.public_key()));
    assert_eq!(shared.as_bytes(), reverse.as_bytes());
    let wrap_key = wrap_key_from_shared(&shared, &ephemeral.public_key(), &recipient.public_key())
        .expect("derive");
    let wrap_cipher = Aes256Gcm::new((&*wrap_key).into());
    let wrapped = wrap_cipher
        .encrypt((&nonce).into(), Payload { msg: &key, aad })
        .expect("wrap");
    let (padded, bucket) = pad_plaintext(plaintext).expect("pad");
    let padded_ciphertext = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: padded.as_slice(),
                aad,
            },
        )
        .expect("padded encrypt");
    let actual = serde_json::json!({
        "key": hex::encode(key), "nonce": hex::encode(nonce),
        "aad": hex::encode(aad), "plaintext": hex::encode(plaintext),
        "ciphertext": hex::encode(&raw[..raw.len() - 16]),
        "tag": hex::encode(&raw[raw.len() - 16..]),
        "recipient_secret": hex::encode(recipient.to_bytes()),
        "recipient_public": hex::encode(recipient.public_key()),
        "ephemeral_secret": hex::encode(ephemeral.to_bytes()),
        "ephemeral_public": hex::encode(ephemeral.public_key()),
        "shared_secret": hex::encode(shared.as_bytes()),
        "hkdf_wrap_key": hex::encode(*wrap_key),
        "wrapped_dek": hex::encode(wrapped),
        "padded_ciphertext": hex::encode(padded_ciphertext), "pad_bucket": bucket
    });
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/rustcrypto-v1.json"))
            .expect("old-code fixture");
    assert_eq!(
        actual, golden,
        "pre-upgrade crypto bytes must remain identical"
    );
    let bytes =
        |field: &str| hex::decode(golden[field].as_str().expect("hex field")).expect("bytes");
    let mut old_raw = bytes("ciphertext");
    old_raw.extend_from_slice(&bytes("tag"));
    assert_eq!(
        cipher
            .decrypt((&nonce).into(), Payload { msg: &old_raw, aad })
            .expect("open old raw ciphertext"),
        plaintext
    );
    let old_wrapped = WrappedDek {
        ephemeral_public: bytes("ephemeral_public").try_into().expect("public key"),
        nonce,
        ciphertext: bytes("wrapped_dek"),
    };
    let dek = unwrap_dek(&old_wrapped, &recipient, aad).expect("open old wrapped DEK");
    assert_eq!(dek.as_bytes(), &key);
    let old_padded = AeadCiphertext {
        alg: AEAD_AES256_GCM_V1,
        nonce,
        ciphertext: bytes("padded_ciphertext"),
        pad_bucket: bucket,
    };
    assert_eq!(
        decrypt_padded(&dek, &old_padded, aad).expect("open old padded ciphertext"),
        plaintext
    );
    for index in [0, old_raw.len() - 1] {
        let mut corrupt = old_raw.clone();
        corrupt[index] ^= 1;
        assert!(
            cipher
                .decrypt((&nonce).into(), Payload { msg: &corrupt, aad })
                .is_err()
        );
    }
    assert!(unwrap_dek(&old_wrapped, &recipient, b"wrong aad").is_err());
    assert!(decrypt_padded(&dek, &old_padded, b"wrong aad").is_err());
}
