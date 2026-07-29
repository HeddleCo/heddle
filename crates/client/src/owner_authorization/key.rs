use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use biscuit_auth::{KeyPair, PrivateKey, builder::Algorithm};
use crypto::{Ed25519Signer, Signer};

use crate::owner_authorization::{
    AuthorizationError, Result,
    canonical::{digest, key_id},
    wire::{AuthorizationKeyAlgorithm, AuthorizationSignature, AuthorizationVerificationKey},
};

/// In-memory Ed25519 authorization signing key.
///
/// Private bytes are never part of any wire object.
pub struct AuthorizationKey {
    seed: [u8; 32],
    signer: Ed25519Signer,
}

impl AuthorizationKey {
    /// Generate a fresh 256-bit Ed25519 seed locally.
    pub fn generate() -> Result<Self> {
        Self::from_seed(rand::random())
    }

    /// Construct a key from an exact 256-bit seed.
    pub fn from_seed(seed: [u8; 32]) -> Result<Self> {
        let signer = Ed25519Signer::from_seed(&seed)?;
        Ok(Self { seed, signer })
    }

    /// Return the public wire representation.
    pub fn verification_key(&self) -> AuthorizationVerificationKey {
        AuthorizationVerificationKey {
            algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
            public_key: self.signer.public_key().to_vec(),
        }
    }

    /// Return the stable key identifier.
    pub fn key_id(&self) -> [u8; 32] {
        key_id(&self.verification_key())
    }

    pub(crate) fn sign(&self, domain: &[u8], body: &[u8]) -> Result<AuthorizationSignature> {
        let message = digest(domain, body);
        Ok(AuthorizationSignature {
            signer_key_id: self.key_id().to_vec(),
            signature: self.signer.sign(&message)?,
        })
    }

    pub(crate) fn biscuit_key_pair(&self) -> Result<KeyPair> {
        let private = PrivateKey::from_bytes(&self.seed, Algorithm::Ed25519)
            .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
        Ok(KeyPair::from(&private))
    }
}

/// Client-generated v1 paper recovery seed for paper or QR export.
pub struct PaperRecoveryKit {
    seed: [u8; 32],
    key: AuthorizationKey,
}

impl PaperRecoveryKit {
    /// Generate a new paper kit locally.
    pub fn generate() -> Result<Self> {
        Self::from_seed(rand::random())
    }

    /// Restore an exact 256-bit paper-kit seed.
    pub fn from_seed(seed: [u8; 32]) -> Result<Self> {
        Ok(Self {
            seed,
            key: AuthorizationKey::from_seed(seed)?,
        })
    }

    /// Restore a URL-safe, unpadded base64 paper/QR encoding.
    pub fn from_base64(value: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|error| {
            AuthorizationError::Invalid(format!("paper recovery seed: {error}"))
        })?;
        let seed = bytes.try_into().map_err(|_| {
            AuthorizationError::Invalid(
                "paper recovery seed must decode to exactly 32 bytes".to_string(),
            )
        })?;
        Self::from_seed(seed)
    }

    /// Encode the 256-bit private seed for paper or QR storage.
    pub fn to_base64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.seed)
    }

    /// Borrow the recovery signer. Only its public key enters wire objects.
    pub fn key(&self) -> &AuthorizationKey {
        &self.key
    }

    /// Consume the printable kit and retain its signer in a recovery setup.
    pub fn into_key(self) -> AuthorizationKey {
        self.key
    }
}

pub(crate) fn verify_signature(
    key: &AuthorizationVerificationKey,
    signature: &AuthorizationSignature,
    domain: &[u8],
    body: &[u8],
) -> Result<()> {
    if key.algorithm != AuthorizationKeyAlgorithm::Ed25519 as i32
        || key.public_key.len() != 32
        || signature.signer_key_id.as_slice() != key_id(key)
        || signature.signature.len() != 64
    {
        return Err(AuthorizationError::InvalidSignature);
    }
    Ed25519Signer::verify_with_public_key(
        &digest(domain, body),
        &key.public_key,
        &signature.signature,
    )
    .map_err(|_| AuthorizationError::InvalidSignature)
}
