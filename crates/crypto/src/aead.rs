// SPDX-License-Identifier: Apache-2.0
//! Versioned random-nonce AEAD and X25519 recipient wrapping (ADR 0051).
//!
//! AES-256-GCM is the single audited symmetric primitive. Each secret uses a
//! fresh DEK; the DEK is wrapped to an X25519 recipient. Encryption keys are
//! never derived from a signing seed.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hmac::{Hmac, KeyInit as HmacKeyInit, Mac};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

type HmacSha256 = Hmac<Sha256>;

/// Domain for HKDF used when wrapping a DEK to an X25519 recipient.
pub const WRAP_HKDF_INFO: &[u8] = b"heddle-runtime-wrap-v1";

/// Version tag stored with ciphertext so algorithms can rotate.
pub const AEAD_AES256_GCM_V1: &str = "aes-256-gcm-v1";

/// Length-prefix + pad buckets. Ciphertext length otherwise tracks plaintext.
pub const PAD_BUCKETS: &[usize] = &[32, 64, 128, 256, 512, 1024, 2048, 4096];

const NONCE_LEN: usize = 12;
const X25519_LEN: usize = 32;
const DEK_LEN: usize = 32;
const LENGTH_PREFIX: usize = 4;

/// 32-byte data-encryption key. Zeroized on drop.
#[derive(Clone)]
pub struct Dek([u8; DEK_LEN]);

impl Drop for Dek {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl Dek {
    pub fn generate() -> Result<Self, AeadError> {
        let mut bytes = [0u8; DEK_LEN];
        fill_random(&mut bytes)?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; DEK_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; DEK_LEN] {
        &self.0
    }
}

/// Exportable X25519 recipient secret. Explicit weaker-custody fallback.
///
/// `StaticSecret` zeroizes on drop. Holding this in-process is weaker custody;
/// the policy broker (later) must hold a provider handle instead.
#[derive(Clone)]
pub struct SoftwareRecipientSecret(StaticSecret);

impl SoftwareRecipientSecret {
    pub fn generate() -> Result<Self, AeadError> {
        let mut seed = [0u8; X25519_LEN];
        fill_random(&mut seed)?;
        Ok(Self(StaticSecret::from(seed)))
    }

    pub fn from_bytes(bytes: [u8; X25519_LEN]) -> Self {
        Self(StaticSecret::from(bytes))
    }

    pub fn to_bytes(&self) -> [u8; X25519_LEN] {
        self.0.to_bytes()
    }

    pub fn public_key(&self) -> [u8; X25519_LEN] {
        PublicKey::from(&self.0).to_bytes()
    }
}

/// Random-nonce AES-256-GCM ciphertext plus the pad bucket used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AeadCiphertext {
    pub alg: &'static str,
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
    pub pad_bucket: u32,
}

/// DEK wrapped to one X25519 recipient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedDek {
    pub ephemeral_public: [u8; X25519_LEN],
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum AeadError {
    #[error("secure random generation failed: {0}")]
    Random(String),
    #[error("aead encryption failed")]
    Encrypt,
    #[error("aead decryption failed")]
    Decrypt,
    #[error("hkdf expansion failed")]
    Hkdf,
    #[error("wrapped dek is truncated")]
    TruncatedWrap,
    #[error("padded plaintext is truncated or corrupt")]
    CorruptPadding,
}

/// Choose the pad bucket for a plaintext length (including the 4-byte prefix).
pub fn pad_bucket_for(plaintext_len: usize) -> usize {
    let needed = plaintext_len.saturating_add(LENGTH_PREFIX);
    for &bucket in PAD_BUCKETS {
        if needed <= bucket {
            return bucket;
        }
    }
    needed.div_ceil(4096).saturating_mul(4096)
}

fn fill_random(dest: &mut [u8]) -> Result<(), AeadError> {
    getrandom::fill(dest).map_err(|err| AeadError::Random(err.to_string()))
}

/// HKDF-SHA256 extract+expand for a single 32-byte OKM (RFC 5869).
fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8]) -> Result<[u8; DEK_LEN], AeadError> {
    let mut extract = <HmacSha256 as HmacKeyInit>::new_from_slice(salt).map_err(|_| AeadError::Hkdf)?;
    extract.update(ikm);
    let prk = extract.finalize().into_bytes();
    let mut expand = <HmacSha256 as HmacKeyInit>::new_from_slice(&prk).map_err(|_| AeadError::Hkdf)?;
    expand.update(info);
    expand.update(&[1u8]);
    let t = expand.finalize().into_bytes();
    let mut okm = [0u8; DEK_LEN];
    if t.len() < DEK_LEN {
        return Err(AeadError::Hkdf);
    }
    okm.copy_from_slice(&t[..DEK_LEN]);
    Ok(okm)
}

fn pad_plaintext(plaintext: &[u8]) -> Result<(Vec<u8>, u32), AeadError> {
    let len = u32::try_from(plaintext.len()).map_err(|_| AeadError::CorruptPadding)?;
    let bucket = pad_bucket_for(plaintext.len());
    let bucket_u32 = u32::try_from(bucket).map_err(|_| AeadError::CorruptPadding)?;
    let mut out = vec![0u8; bucket];
    out[..LENGTH_PREFIX].copy_from_slice(&len.to_be_bytes());
    let end = LENGTH_PREFIX + plaintext.len();
    if end > bucket {
        return Err(AeadError::CorruptPadding);
    }
    out[LENGTH_PREFIX..end].copy_from_slice(plaintext);
    Ok((out, bucket_u32))
}

fn unpad_plaintext(padded: &[u8]) -> Result<Vec<u8>, AeadError> {
    if padded.len() < LENGTH_PREFIX {
        return Err(AeadError::CorruptPadding);
    }
    let mut len_bytes = [0u8; LENGTH_PREFIX];
    len_bytes.copy_from_slice(&padded[..LENGTH_PREFIX]);
    let len = usize::try_from(u32::from_be_bytes(len_bytes)).unwrap_or(usize::MAX);
    let end = LENGTH_PREFIX.saturating_add(len);
    if end > padded.len() {
        return Err(AeadError::CorruptPadding);
    }
    if padded[end..].iter().any(|byte| *byte != 0) {
        return Err(AeadError::CorruptPadding);
    }
    Ok(padded[LENGTH_PREFIX..end].to_vec())
}

/// Encrypt `plaintext` under `dek` with a fresh random nonce. `aad` binds the
/// ciphertext to a slot/profile so it cannot be replayed onto another record.
pub fn encrypt_padded(
    dek: &Dek,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<AeadCiphertext, AeadError> {
    let (padded, pad_bucket) = pad_plaintext(plaintext)?;
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(dek.as_bytes()));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &padded, aad })
        .map_err(|_| AeadError::Encrypt)?;
    Ok(AeadCiphertext {
        alg: AEAD_AES256_GCM_V1,
        nonce,
        ciphertext,
        pad_bucket,
    })
}

pub fn decrypt_padded(
    dek: &Dek,
    sealed: &AeadCiphertext,
    aad: &[u8],
) -> Result<Vec<u8>, AeadError> {
    if sealed.alg != AEAD_AES256_GCM_V1 {
        return Err(AeadError::Decrypt);
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(dek.as_bytes()));
    let padded = cipher
        .decrypt(
            Nonce::from_slice(&sealed.nonce),
            Payload {
                msg: &sealed.ciphertext,
                aad,
            },
        )
        .map_err(|_| AeadError::Decrypt)?;
    if padded.len() != sealed.pad_bucket as usize {
        return Err(AeadError::CorruptPadding);
    }
    unpad_plaintext(&padded)
}

fn wrap_key(
    ephemeral_secret: &StaticSecret,
    recipient_public: &[u8; X25519_LEN],
) -> Result<[u8; DEK_LEN], AeadError> {
    let shared = ephemeral_secret.diffie_hellman(&PublicKey::from(*recipient_public));
    let ephemeral_public = PublicKey::from(ephemeral_secret).to_bytes();
    let mut salt = [0u8; X25519_LEN * 2];
    salt[..X25519_LEN].copy_from_slice(&ephemeral_public);
    salt[X25519_LEN..].copy_from_slice(recipient_public);
    hkdf_sha256(&salt, shared.as_bytes(), WRAP_HKDF_INFO)
}

/// Wrap `dek` to `recipient_public` with an ephemeral X25519 key.
pub fn wrap_dek(dek: &Dek, recipient_public: &[u8; X25519_LEN]) -> Result<WrappedDek, AeadError> {
    let ephemeral = SoftwareRecipientSecret::generate()?;
    let wrap_key = wrap_key(&ephemeral.0, recipient_public)?;
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&wrap_key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), dek.as_bytes().as_slice())
        .map_err(|_| AeadError::Encrypt)?;
    Ok(WrappedDek {
        ephemeral_public: ephemeral.public_key(),
        nonce,
        ciphertext,
    })
}

pub fn unwrap_dek(
    wrapped: &WrappedDek,
    recipient: &SoftwareRecipientSecret,
) -> Result<Dek, AeadError> {
    if wrapped.ciphertext.len() < 16 {
        return Err(AeadError::TruncatedWrap);
    }
    let ephemeral_secret_for_shared = &recipient.0;
    let shared =
        ephemeral_secret_for_shared.diffie_hellman(&PublicKey::from(wrapped.ephemeral_public));
    let recipient_public = recipient.public_key();
    let mut salt = [0u8; X25519_LEN * 2];
    salt[..X25519_LEN].copy_from_slice(&wrapped.ephemeral_public);
    salt[X25519_LEN..].copy_from_slice(&recipient_public);
    let okm = hkdf_sha256(&salt, shared.as_bytes(), WRAP_HKDF_INFO)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&okm));
    let dek_bytes = cipher
        .decrypt(
            Nonce::from_slice(&wrapped.nonce),
            wrapped.ciphertext.as_slice(),
        )
        .map_err(|_| AeadError::Decrypt)?;
    let dek_arr: [u8; DEK_LEN] = dek_bytes.try_into().map_err(|_| AeadError::TruncatedWrap)?;
    Ok(Dek::from_bytes(dek_arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_round_trip_hides_exact_length_inside_a_bucket() {
        let dek = Dek::generate().expect("dek");
        let sealed = encrypt_padded(&dek, b"secret-value", b"aad-v1").expect("encrypt");
        assert_eq!(sealed.alg, AEAD_AES256_GCM_V1);
        assert_eq!(sealed.pad_bucket, 32);
        assert_ne!(&sealed.ciphertext, b"secret-value");
        let plain = decrypt_padded(&dek, &sealed, b"aad-v1").expect("decrypt");
        assert_eq!(plain, b"secret-value");
    }

    #[test]
    fn wrong_aad_cannot_decrypt() {
        let dek = Dek::generate().expect("dek");
        let sealed = encrypt_padded(&dek, b"secret-value", b"slot-a").expect("encrypt");
        decrypt_padded(&dek, &sealed, b"slot-b").expect_err("aad mismatch");
    }

    #[test]
    fn wrap_round_trip_and_wrong_recipient_fails() {
        let dek = Dek::generate().expect("dek");
        let alice = SoftwareRecipientSecret::generate().expect("alice");
        let bob = SoftwareRecipientSecret::generate().expect("bob");
        let wrapped = wrap_dek(&dek, &alice.public_key()).expect("wrap");
        let opened = unwrap_dek(&wrapped, &alice).expect("alice unwraps");
        assert_eq!(opened.as_bytes(), dek.as_bytes());
        unwrap_dek(&wrapped, &bob).expect_err("bob cannot unwrap");
    }

    #[test]
    fn pad_buckets_jump_to_4k_increments_after_4k() {
        assert_eq!(pad_bucket_for(0), 32);
        assert_eq!(pad_bucket_for(28), 32);
        assert_eq!(pad_bucket_for(29), 64);
        assert_eq!(pad_bucket_for(4093), 8192);
    }
}
