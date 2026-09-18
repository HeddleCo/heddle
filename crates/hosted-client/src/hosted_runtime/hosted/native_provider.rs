//! Native provider consent uses the active hosted credential's terminal PoP
//! key and this connection's actual Iroh client peer. CLI callers never supply
//! a separate signer or asserted subject for one Fetch.

use std::{collections::BTreeMap, path::Path, time::Duration};

use api::heddle::api::v2alpha1::{
    EndpointKind, EndpointRef, FetchOpen, ProviderDialRoute, ProviderPlan, fetch_open,
};
use crypto::Signer as _;
use thread_api::{
    Remote,
    credentials::Credentials,
    fetch::{Error as FetchError, Limits, ProviderConsentSigner, ProviderFetch, StagedSource},
    transport::IrohTransport,
};

use super::{CallContextFactory, HostedClient, HostedError, Result};

const PROVIDER_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROVIDER_ROUTES: usize = 128;

pub(super) struct NativeProviderConsent<'a> {
    context: &'a CallContextFactory,
    subject: String,
    client: EndpointRef,
}

/// Select provider-preferred Fetch only when the opening carries usable dial
/// routes. Empty or invalid routes leave the existing direct opening unchanged.
pub(super) fn preferred_fetch_open(
    mut open: FetchOpen,
    routes: Vec<ProviderDialRoute>,
) -> FetchOpen {
    if validate_routes(&routes).is_ok() {
        open.delivery = fetch_open::Delivery::ProviderPreferred as i32;
        open.routes = routes;
    }
    open
}

fn direct_fetch_open(open: &FetchOpen) -> FetchOpen {
    let mut open = open.clone();
    open.delivery = fetch_open::Delivery::Direct as i32;
    open.routes.clear();
    open
}

impl HostedClient {
    /// Download one exact native source, using provider delivery only when the
    /// authenticated Fetch admission selects it. Provider routes remain hints:
    /// every selected endpoint is proved by Iroh and DescribeEndpoint before a
    /// ticketed range is read. Provider-preferred means preferred with fallback:
    /// missing routes or a failed negotiation still complete over direct Fetch.
    pub async fn fetch_native_source(
        &self,
        open: FetchOpen,
        limits: Limits,
        scratch: &Path,
    ) -> anyhow::Result<StagedSource> {
        let delivery = fetch_open::Delivery::try_from(open.delivery)
            .map_err(|_| FetchError::Invalid("unsupported Fetch delivery"))?;
        let remote = self.native().await?;
        if delivery != fetch_open::Delivery::ProviderPreferred
            || validate_routes(&open.routes).is_err()
        {
            let open = if delivery == fetch_open::Delivery::ProviderPreferred {
                direct_fetch_open(&open)
            } else {
                open
            };
            return Ok(remote
                .fetch_content(open, limits)
                .await?
                .stage(scratch)
                .await?);
        }

        let routes = open.routes.clone();
        match self
            .fetch_preferred_source(&remote, open.clone(), limits, scratch, &routes)
            .await
        {
            Ok(staged) => Ok(staged),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "preferred provider Fetch failed; using direct source transfer"
                );
                Ok(remote
                    .fetch_content(direct_fetch_open(&open), limits)
                    .await?
                    .stage(scratch)
                    .await?)
            }
        }
    }

    async fn fetch_preferred_source(
        &self,
        remote: &Remote<IrohTransport<Credentials>>,
        open: FetchOpen,
        limits: Limits,
        scratch: &Path,
        routes: &[ProviderDialRoute],
    ) -> anyhow::Result<StagedSource> {
        match remote.begin_provider_fetch(open, limits).await? {
            ProviderFetch::Direct(download) => Ok((*download).stage(scratch).await?),
            ProviderFetch::Provider(download) => {
                let signer = self.provider_consent()?;
                let mut session = (*download).negotiate(&signer).await?;
                session.receive_inline(scratch).await?;
                let providers = self.native_provider_remotes(session.plan(), routes).await?;
                session.receive_provider_ranges(&providers).await?;
                Ok(session.complete(scratch).await?)
            }
        }
    }

    pub(super) fn provider_consent(&self) -> Result<NativeProviderConsent<'_>> {
        let signer = self
            .context
            .proof_signer()
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

    async fn native_provider_remotes(
        &self,
        plan: &ProviderPlan,
        routes: &[ProviderDialRoute],
    ) -> anyhow::Result<Vec<Remote<IrohTransport<Credentials>>>> {
        let mut selected = BTreeMap::new();
        for extent in &plan.extents {
            let provider = extent
                .provider
                .as_ref()
                .ok_or(FetchError::Invalid("provider endpoint absent"))?;
            let key = provider_key(provider)?;
            if let Some(previous) = selected.insert(key, provider.clone())
                && previous != *provider
            {
                return Err(FetchError::Invalid("provider endpoint identity conflicts").into());
            }
        }
        let credentials = self.context.native_provider_credentials()?;
        let mut providers = Vec::with_capacity(selected.len());
        for (key, provider) in selected {
            let connection = self
                .connection
                .native_provider_connection(&provider, routes)
                .await?;
            let transport = IrohTransport::new(
                connection,
                credentials.clone(),
                thread_api::replication::opening::FRAME_LIMIT,
                PROVIDER_PROGRESS_TIMEOUT,
            )?;
            providers.push(Remote::discover(transport, key, EndpointKind::Provider).await?);
        }
        Ok(providers)
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
        self.context
            .proof_signer()
            .map_or(&[], |signer| signer.public_key())
    }

    fn sign(&self, canonical: &[u8]) -> std::result::Result<Vec<u8>, FetchError> {
        self.context
            .proof_signer()
            .ok_or(FetchError::Invalid("provider consent signer unavailable"))?
            .sign(canonical)
            .map_err(|error| FetchError::Preparation(error.to_string()))
    }
}

fn validate_routes(routes: &[ProviderDialRoute]) -> std::result::Result<(), FetchError> {
    if routes.is_empty() || routes.len() > MAX_PROVIDER_ROUTES {
        return Err(FetchError::Invalid(
            "preferred provider Fetch requires bounded dial routes",
        ));
    }
    for route in routes {
        provider_key(
            route
                .provider
                .as_ref()
                .ok_or(FetchError::Invalid("provider dial endpoint absent"))?,
        )?;
        if route.address.is_none() {
            return Err(FetchError::Invalid("provider dial address absent"));
        }
    }
    Ok(())
}

fn provider_key(provider: &EndpointRef) -> std::result::Result<[u8; 32], FetchError> {
    if provider.kind != EndpointKind::Provider as i32 {
        return Err(FetchError::Invalid(
            "provider endpoint kind must be provider",
        ));
    }
    provider
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| FetchError::Invalid("provider endpoint key must be 32 bytes"))
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v2alpha1::{
        EndpointKind, EndpointRef, FetchOpen, ProviderDialRoute, fetch_open, provider_dial_route,
    };

    use super::{MAX_PROVIDER_ROUTES, preferred_fetch_open, validate_routes};

    fn valid_route() -> ProviderDialRoute {
        ProviderDialRoute {
            provider: Some(EndpointRef {
                public_key: vec![7; 32],
                kind: EndpointKind::Provider as i32,
            }),
            address: Some(provider_dial_route::Address::RelayUrl(
                "https://relay.example/".to_string(),
            )),
        }
    }

    #[test]
    fn provider_route_validation_reuses_the_fetch_contract() {
        let route = valid_route();
        validate_routes(std::slice::from_ref(&route)).expect("valid provider Fetch route");

        let mut missing_address = route.clone();
        missing_address.address = None;
        assert!(validate_routes(&[missing_address]).is_err());

        let mut wrong_kind = route.clone();
        wrong_kind.provider = Some(EndpointRef {
            kind: EndpointKind::Device as i32,
            public_key: vec![7; 32],
        });
        assert!(validate_routes(&[wrong_kind]).is_err());
        assert!(validate_routes(&[]).is_err());
        assert!(validate_routes(&vec![route; MAX_PROVIDER_ROUTES + 1]).is_err());
    }

    #[test]
    fn preferred_fetch_open_selects_provider_only_when_routes_are_usable() {
        let open = FetchOpen::default();
        let without_routes = preferred_fetch_open(open.clone(), Vec::new());
        assert_ne!(
            without_routes.delivery,
            fetch_open::Delivery::ProviderPreferred as i32
        );
        assert!(without_routes.routes.is_empty());

        let with_routes = preferred_fetch_open(open, vec![valid_route()]);
        assert_eq!(
            with_routes.delivery,
            fetch_open::Delivery::ProviderPreferred as i32
        );
        assert_eq!(with_routes.routes.len(), 1);
    }
}
