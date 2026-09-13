//! Native provider consent uses the active hosted credential's terminal PoP
//! key and this connection's actual Iroh client peer. CLI callers never supply
//! a separate signer or asserted subject for one Fetch.

use api::heddle::api::v2alpha1::{EndpointKind, EndpointRef};
use crypto::Signer as _;
use thread_api::fetch::{Error as FetchError, ProviderConsentSigner};

use super::{CallContextFactory, HostedClient, HostedError, Result};

pub(super) struct NativeProviderConsent<'a> {
    context: &'a CallContextFactory,
    subject: String,
    client: EndpointRef,
}

impl HostedClient {
    pub(super) fn provider_consent(&self) -> Result<NativeProviderConsent<'_>> {
        let signer = self.context.proof_signer()
            .ok_or(HostedError::SigningIdentityRequired)?;
        let token = std::str::from_utf8(self.context.bearer_capability())
            .map_err(|error| HostedError::Framing(error.to_string()))?;
        let subject = crate::hosted_runtime::device_flow::authenticated_subject(token)
            .map_err(|error| HostedError::Framing(error.to_string()))?;
        let effective = crate::hosted_runtime::device_flow::effective_pop_public_key_hex(token)
            .map_err(|error| HostedError::Framing(error.to_string()))?;
        if !effective.eq_ignore_ascii_case(&hex::encode(signer.public_key())) {
            return Err(HostedError::SigningIdentityRequired);
        }
        Ok(NativeProviderConsent {
            context: &self.context,
            subject,
            client: EndpointRef {
                public_key: self.connection.endpoint_id().as_bytes().to_vec(),
                kind: EndpointKind::Device as i32,
            },
        })
    }
}

impl ProviderConsentSigner for NativeProviderConsent<'_> {
    fn verified_subject(&self) -> std::result::Result<String, FetchError> {
        Ok(self.subject.clone())
    }

    fn client_endpoint(&self) -> std::result::Result<EndpointRef, FetchError> {
        Ok(self.client.clone())
    }

    fn public_key(&self) -> &[u8] {
        self.context.proof_signer().map_or(&[], |signer| signer.public_key())
    }

    fn sign(&self, canonical: &[u8]) -> std::result::Result<Vec<u8>, FetchError> {
        self.context.proof_signer()
            .ok_or(FetchError::Invalid("provider consent signer unavailable"))?
            .sign(canonical)
            .map_err(|error| FetchError::Preparation(error.to_string()))
    }
}
