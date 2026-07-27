use base64::Engine as _;
use sha2::{Digest, Sha256};
use wire::ProtocolError;

#[derive(Clone, Debug)]
pub struct WebAuthnAssertion {
    pub credential_id: Vec<u8>,
    pub signature: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub user_handle: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct HumanSignatureRequest {
    pub method_path: String,
    pub action_summary: String,
    pub challenge: String,
    pub canonical: Vec<u8>,
    pub action_url: Option<String>,
}

pub type HumanSignatureCallback = std::sync::Arc<
    dyn Fn(HumanSignatureRequest) -> Result<WebAuthnAssertion, ProtocolError> + Send + Sync,
>;

pub(super) fn challenge(canonical: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(canonical))
}
