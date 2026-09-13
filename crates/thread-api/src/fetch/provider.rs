//! Capability-free candidate admission followed by exact signed consent.
//! The caller's verified credential implementation owns its terminal PoP key;
//! this module never accepts an arbitrary identity string or private key per
//! Fetch invocation.

use api::{
    heddle::api::v2alpha1::{
        EndpointRef, FetchClientFrame, FetchOpen, FetchServerFrame, ProviderConsent, ProviderOffer,
        ProviderPlan, ProviderPlanChallenge, RecordSignature, SignedRecord, fetch_client_frame,
        fetch_open, fetch_server_frame,
    },
    provider_v2::{
        PROVIDER_CONSENT_FORMAT, provider_consent_signing_bytes, validate_plan_for_offer,
        validate_provider_offer,
    },
    v2::client::{MessageReader, MessageWriter, Messages, RpcTransport, Sender},
};

use super::{Error, Item, Limits, Validation};
use crate::{Remote, contract::TransferReady, rpc, transport};

/// Implemented by the same credential that signed the Fetch opening. The
/// server verifies this identity against its authenticated Biscuit subject
/// and requires the signature key to equal the credential's terminal cnf key.
pub trait ProviderConsentSigner {
    fn verified_subject(&self) -> Result<String, Error>;
    fn client_endpoint(&self) -> Result<EndpointRef, Error>;
    fn public_key(&self) -> &[u8];
    fn sign(&self, canonical: &[u8]) -> Result<Vec<u8>, Error>;
}

/// An authenticated Fetch exchange whose request half remains open for exact
/// consent and the final verified result. Dropping it aborts both halves.
pub struct ProviderDownload<
    W: MessageWriter<Error = transport::Error>,
    R: MessageReader<Error = transport::Error>,
> {
    sender: Sender<W, FetchClientFrame>,
    messages: Messages<R, FetchServerFrame>,
    state: Validation,
    open: FetchOpen,
    issuer: EndpointRef,
}

/// Only an issued plan matching the signed candidate can reach this stage.
pub struct ProviderPlanSession<
    W: MessageWriter<Error = transport::Error>,
    R: MessageReader<Error = transport::Error>,
> {
    pub plan: ProviderPlan,
    pub ready: TransferReady,
    pub originals: Vec<Item>,
    sender: Sender<W, FetchClientFrame>,
    messages: Messages<R, FetchServerFrame>,
    state: Validation,
}

impl<T: RpcTransport<Error = transport::Error>> Remote<T> {
    pub async fn begin_provider_fetch(
        &self,
        open: FetchOpen,
        limits: Limits,
    ) -> Result<ProviderDownload<T::Writer, T::Reader>, Error> {
        if open.delivery != fetch_open::Delivery::ProviderPreferred as i32
            || open.checkpoint.is_some()
        {
            return Err(Error::Invalid("fresh preferred provider Fetch required"));
        }
        let issuer = self
            .description
            .endpoint
            .clone()
            .ok_or(Error::Invalid("issuer endpoint required"))?;
        let (sender, mut messages) = self
            .api
            .exchange::<rpc::SyncServiceFetch>(&FetchClientFrame {
                body: Some(fetch_client_frame::Body::Open(open.clone())),
            })
            .await?;
        let frame = messages
            .next()
            .await?
            .ok_or(Error::Invalid("provider Ready required"))?;
        let Some(fetch_server_frame::Body::Ready(ready)) = frame.body else {
            return Err(Error::Invalid("first provider response must be Ready"));
        };
        let state = Validation::new(open.clone(), ready, Some(&issuer), limits)?;
        Ok(ProviderDownload {
            sender,
            messages,
            state,
            open,
            issuer,
        })
    }
}

impl<W: MessageWriter<Error = transport::Error>, R: MessageReader<Error = transport::Error>>
    ProviderDownload<W, R>
{
    pub async fn negotiate(
        mut self,
        signer: &impl ProviderConsentSigner,
    ) -> Result<ProviderPlanSession<W, R>, Error> {
        let mut originals = Vec::new();
        let offer = loop {
            let frame = self
                .messages
                .next()
                .await?
                .ok_or(Error::Invalid("provider Offer required"))?;
            match frame.body {
                Some(fetch_server_frame::Body::Operations(_))
                | Some(fetch_server_frame::Body::ThreadGenesis(_)) => {
                    originals.push(self.state.accept(frame)?);
                }
                Some(fetch_server_frame::Body::ProviderOffer(offer)) => break offer,
                _ => return Err(Error::Invalid("unexpected frame before provider Offer")),
            }
        };
        if self
            .state
            .ready
            .checkpoint
            .as_ref()
            .is_none_or(|checkpoint| checkpoint.plan_digest != offer.assembly_digest)
        {
            return Err(Error::Invalid(
                "provider Offer differs from Ready checkpoint",
            ));
        }
        let candidate =
            Candidate::new(&self.open, &self.issuer, &signer.client_endpoint()?, offer)?;
        self.sender
            .send(&FetchClientFrame {
                body: Some(fetch_client_frame::Body::Consent(
                    candidate.consent(signer)?,
                )),
            })
            .await?;
        let issued = self
            .messages
            .next()
            .await?
            .ok_or(Error::Invalid("issued provider Plan required"))?;
        let Some(fetch_server_frame::Body::ProviderPlan(plan)) = issued.body else {
            return Err(Error::Invalid(
                "first frame after consent must be provider Plan",
            ));
        };
        candidate.admit(&plan)?;
        Ok(ProviderPlanSession {
            ready: self.state.ready.clone(),
            plan,
            originals,
            sender: self.sender,
            messages: self.messages,
            state: self.state,
        })
    }
}

/// An unsigned offer bound to the authenticated issuer, client, and exact
/// selected source. It grants no provider read until final ticket admission.
pub struct Candidate {
    offer: ProviderOffer,
}

impl Candidate {
    pub fn new(
        open: &FetchOpen,
        issuer: &EndpointRef,
        client: &EndpointRef,
        offer: ProviderOffer,
    ) -> Result<Self, Error> {
        if open.delivery != fetch_open::Delivery::ProviderPreferred as i32 {
            return Err(Error::Invalid("provider offer requires preferred delivery"));
        }
        validate_provider_offer(&offer).map_err(|_| Error::Invalid("invalid provider offer"))?;
        let challenge = offer
            .challenge
            .as_ref()
            .ok_or(Error::Invalid("provider challenge required"))?;
        if challenge.thread != open.thread
            || open
                .revision
                .as_ref()
                .is_some_and(|revision| challenge.revision.as_ref() != Some(revision))
            || challenge.issuer.as_ref() != Some(issuer)
            || challenge.client.as_ref() != Some(client)
        {
            return Err(Error::Invalid(
                "provider offer differs from selected source or peer",
            ));
        }
        let expiry = challenge
            .expires_at
            .as_ref()
            .ok_or(Error::Invalid("provider expiry required"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Invalid("provider clock unavailable"))?;
        if expiry.seconds
            <= i64::try_from(now.as_secs())
                .map_err(|_| Error::Invalid("provider clock overflow"))?
        {
            return Err(Error::Invalid("provider offer expired"));
        }
        Ok(Self { offer })
    }

    pub fn challenge(&self) -> Result<&ProviderPlanChallenge, Error> {
        self.offer
            .challenge
            .as_ref()
            .ok_or(Error::Invalid("validated provider challenge absent"))
    }

    pub fn consent(&self, signer: &impl ProviderConsentSigner) -> Result<ProviderConsent, Error> {
        let identity = format!("principal:{}", signer.verified_subject()?);
        let canonical = provider_consent_signing_bytes(self.challenge()?, &identity)
            .map_err(|_| Error::Invalid("invalid provider consent challenge"))?;
        let key = signer.public_key();
        if key.len() != 32 {
            return Err(Error::Invalid("provider consent key must be Ed25519"));
        }
        let signature = signer.sign(&canonical)?;
        if signature.len() != 64 {
            return Err(Error::Invalid("provider consent signature length"));
        }
        Ok(ProviderConsent {
            extent_set_digest: self.offer.extent_set_digest.clone(),
            exact_plan_consent: Some(SignedRecord {
                format: PROVIDER_CONSENT_FORMAT.into(),
                canonical_record: canonical,
                signatures: vec![RecordSignature {
                    public_key: key.to_vec(),
                    signature,
                }],
            }),
            assembly_digest: self.offer.assembly_digest.clone(),
        })
    }

    pub fn admit(&self, plan: &ProviderPlan) -> Result<(), Error> {
        validate_plan_for_offer(&self.offer, plan)
            .map_err(|_| Error::Invalid("issued provider plan differs from consented offer"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ready_has_no_direct_pack_or_partial_fallback() {
        let (mut open, mut ready, endpoint, _) = super::super::tests::fixture();
        open.delivery = fetch_open::Delivery::ProviderPreferred as i32;
        assert!(
            Validation::new(
                open.clone(),
                ready.clone(),
                Some(&endpoint),
                Limits::default()
            )
            .is_err(),
            "provider mode cannot silently receive direct source packs"
        );
        ready.packs.clear();
        Validation::new(
            open.clone(),
            ready.clone(),
            Some(&endpoint),
            Limits::default(),
        )
        .expect("complete provider Ready with no direct artifact");
        ready.full_closure_available = false;
        assert!(
            Validation::new(open, ready, Some(&endpoint), Limits::default()).is_err(),
            "provider offer cannot downgrade whole-source disclosure"
        );
    }
}
