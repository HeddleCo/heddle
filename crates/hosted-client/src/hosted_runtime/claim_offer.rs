//! Resident agent side of the human claim ceremony.

// The transport contract owns `CallFailure` by value at the owner-root
// seam (`handle_owner_root_body`), matching `claim_authorization` and
// `claim_protocol`; boxing it would only fragment that seam's error type.
#![allow(clippy::result_large_err)]

use std::time::Duration;

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{CallFailure, CallFailureCode};
use config::UserConfig;

use super::{
    HostedAuthMode, HostedSession, agent_node_identity,
    auth::resolve_server,
    claim_authorization::validate_stored_claim_signer,
    claim_bridge::ClaimBridgeWorker,
    hosted::{
        canonical_server_authority, claim_protocol::VerifiedClaimPrincipal, server_keys_match,
    },
    identity_state::{self, ClaimIssuanceStatus, ClaimSecret, ClaimState},
};

const HEDDLE_SAAS_API: &str = "https://api.heddle.sh";

const CLAIM_STATUS_POLL: Duration = Duration::from_millis(200);
const HEDDLE_SAAS_CLAIM_ORIGIN: &str = "https://app.heddle.sh";

/// Resolved inputs for the hosted claim ceremony.
#[derive(Clone, Debug)]
pub struct ClaimOptions {
    pub server: Option<String>,
    pub web_origin: Option<String>,
    pub timeout: Duration,
}

/// A short-lived claim offer that must be shown to the human before waiting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimOfferReady {
    pub pet_name: String,
    pub claim_link: String,
    pub timeout: Duration,
}

/// Terminal state of a claim ceremony.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    ConsentIssued,
    Expired,
    Interrupted,
}

struct ActiveClaimOffer {
    secret: ClaimSecret,
    authorization_hash: String,
    node_id: String,
    pet_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClaimWaitOutcome {
    ConsentIssued,
    Expired,
    Interrupted,
    Replaced,
}

/// Run the claim ceremony without owning its presentation.
///
/// `on_ready` runs once after the short-lived offer is active and before the
/// operation waits for a human. Callers can render it, forward it as an event,
/// or retain it for another surface.
pub async fn claim(
    args: ClaimOptions,
    on_ready: impl FnOnce(&ClaimOfferReady) -> Result<()>,
) -> Result<ClaimOutcome> {
    let server = resolve_server(args.server.as_deref())?;
    let state = claimable_state(&server)?;
    // Bind the destination before activate_offer mints the local bearer.
    let web_origin = resolve_web_origin(
        args.web_origin.as_deref(),
        state.web_origin.as_deref(),
        &server,
    )?;
    validate_stored_claim_signer(&state)?;
    let relay_url = crate::network::claim_relay_url(&state.node_id)?;

    let identity = agent_node_identity::load()?
        .context("no agent node identity exists; create an agent account with `heddle auth login --invite <code>` first")?;
    if identity.node_id().to_string() != state.node_id {
        bail!("agent-node-identity.toml does not match the account waiting to be claimed");
    }

    // The persistent box network daemon serves the native v2
    // router on this device node id (heddle#1620). A foreground
    // `heddle claim` does not host the endpoint or the router; it arms
    // itself as the owner-root co-sign signer over the daemon's UDS
    // bridge and observes the shared, file-backed claim state.
    let heddle_home = repo::identity::heddle_home_dir();
    let claim_socket = crate::hosted_runtime::claim_bridge::claim_bridge_socket_path(&heddle_home);

    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    // Outbound only: an ephemeral endpoint that does not bind the device
    // node id the daemon serves the claim router on.
    let client = session
        .connect_outbound(&server)
        .await
        .with_context(|| format!("connecting to {server} for the claim ceremony"))?;
    // Install the claimable seq-0 root before the Iroh ceremony so claim works
    // even if this account never created a project spool. weft#2041 (Option C):
    // the stored credential cannot install an owner root, so pin over a full
    // root re-minted in memory from the node seed rather than this connection's
    // restricted bearer.
    if let Err(error) = super::auth_login_agent::ensure_owner_root_pinned(&server).await {
        client.close().await;
        return Err(error);
    }

    let offer = match activate_offer(&state, args.timeout) {
        Ok(offer) => offer,
        Err(error) => {
            client.close().await;
            return Err(error);
        }
    };
    let claim_link = claim_link(
        &web_origin,
        &offer.node_id,
        offer.secret.as_str(),
        &relay_url,
    )?;
    let ready = ClaimOfferReady {
        pet_name: offer.pet_name.clone(),
        claim_link,
        timeout: args.timeout,
    };
    if let Err(error) = on_ready(&ready) {
        client.close().await;
        deactivate_offer(&offer.authorization_hash)?;
        return Err(error);
    }
    drop(ready);
    drop(offer.secret);

    let outcome = wait_for_claim(
        &offer.authorization_hash,
        args.timeout,
        &claim_socket,
        &client,
    )
    .await;
    let claimed = matches!(outcome, Ok(ClaimWaitOutcome::ConsentIssued));
    let cleanup = (!claimed)
        .then(|| deactivate_offer(&offer.authorization_hash))
        .transpose();
    client.close().await;
    cleanup?;

    match outcome? {
        // Consent preserves the original root. The browser completes
        // registration directly with Weft using this signed transition.
        ClaimWaitOutcome::ConsentIssued => Ok(ClaimOutcome::ConsentIssued),
        ClaimWaitOutcome::Expired => Ok(ClaimOutcome::Expired),
        ClaimWaitOutcome::Interrupted => Ok(ClaimOutcome::Interrupted),
        ClaimWaitOutcome::Replaced => {
            bail!("this claim offer was replaced by another `heddle claim` process")
        }
    }
}

fn claimable_state(server: &str) -> Result<ClaimState> {
    let state = identity_state::load()?.context(
        "no agent account is waiting to be claimed; create one with `heddle auth login --invite <code>` first",
    )?;
    if !server_keys_match(&state.server, server) {
        bail!(
            "the stored agent account belongs to {}, but `heddle claim` targets {server}",
            state.server
        );
    }
    if state.consent_issued() {
        bail!("owner consent has already been issued; finish the existing browser ceremony");
    }
    Ok(state)
}

fn activate_offer(expected: &ClaimState, timeout: Duration) -> Result<ActiveClaimOffer> {
    let timeout_millis = i64::try_from(timeout.as_millis())
        .context("claim timeout is too large for a millisecond timestamp")?;
    let expires_at_millis = chrono::Utc::now()
        .timestamp_millis()
        .checked_add(timeout_millis)
        .context("claim expiry overflows the timestamp range")?;
    let _guard = identity_state::write_lock()?;
    let mut state = identity_state::load_while_locked()?
        .context("agent claim state disappeared before the offer was activated")?;
    if state.owner_id != expected.owner_id
        || state.node_id != expected.node_id
        || !server_keys_match(&state.server, &expected.server)
    {
        bail!("agent claim state changed before the offer was activated");
    }
    let secret = state
        .activate(expires_at_millis)?
        .context("owner consent has already been issued; finish the existing browser ceremony")?;
    let offer = ActiveClaimOffer {
        secret,
        authorization_hash: state.authorization_hash().to_string(),
        node_id: state.node_id.clone(),
        pet_name: state.pet_name.clone(),
    };
    identity_state::store_while_locked(&state)?;
    Ok(offer)
}

async fn wait_for_claim(
    authorization_hash: &str,
    timeout: Duration,
    claim_socket: &std::path::Path,
    client: &super::hosted::HostedClient,
) -> Result<ClaimWaitOutcome> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(CLAIM_STATUS_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Arm as the daemon-hosted router's owner-root co-sign signer for this
    // window. The daemon drives native claim admission itself
    // and forwards only the owner-root co-sign here, where the signer lives.
    let mut worker = ClaimBridgeWorker::arm(claim_socket).await.context(
        "arming the owner-root co-sign bridge; is `heddle netd serve` running on this machine?",
    )?;
    loop {
        tokio::select! {
            biased;

            served = worker.serve_next(client) => {
                let alive = served.unwrap_or_else(|error| {
                    tracing::warn!(%error, "owner-root co-sign bridge failed; will re-arm");
                    false
                });
                if !alive {
                    // The daemon closed the bridge — it may be restarting
                    // mid-window. Re-arm so a re-dial after the router
                    // re-mounts still reaches this foreground signer.
                    match rearm(claim_socket, authorization_hash, deadline).await? {
                        RearmOutcome::Rearmed(next) => worker = next,
                        RearmOutcome::Terminal(outcome) => return Ok(outcome),
                    }
                }
            }
            result = tokio::signal::ctrl_c() => {
                result.context("waiting for Ctrl-C")?;
                return Ok(ClaimWaitOutcome::Interrupted);
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(ClaimWaitOutcome::Expired);
            }
            _ = poll.tick() => {
                match observe_issuance(authorization_hash)? {
                    ClaimIssuanceStatus::Active => {}
                    ClaimIssuanceStatus::ConsentIssued => return Ok(ClaimWaitOutcome::ConsentIssued),
                    ClaimIssuanceStatus::Expired => return Ok(ClaimWaitOutcome::Expired),
                    ClaimIssuanceStatus::Replaced => return Ok(ClaimWaitOutcome::Replaced),
                }
            }
        }
    }
}

enum RearmOutcome {
    Rearmed(ClaimBridgeWorker),
    Terminal(ClaimWaitOutcome),
}

/// Re-establish the co-sign bridge after the daemon closed it (a
/// mid-window restart), or report a terminal outcome if the ceremony
/// resolved or the deadline passed while the daemon was away.
async fn rearm(
    claim_socket: &std::path::Path,
    authorization_hash: &str,
    deadline: tokio::time::Instant,
) -> Result<RearmOutcome> {
    loop {
        match observe_issuance(authorization_hash)? {
            ClaimIssuanceStatus::Active => {}
            ClaimIssuanceStatus::ConsentIssued => {
                return Ok(RearmOutcome::Terminal(ClaimWaitOutcome::ConsentIssued));
            }
            ClaimIssuanceStatus::Expired => {
                return Ok(RearmOutcome::Terminal(ClaimWaitOutcome::Expired));
            }
            ClaimIssuanceStatus::Replaced => {
                return Ok(RearmOutcome::Terminal(ClaimWaitOutcome::Replaced));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(RearmOutcome::Terminal(ClaimWaitOutcome::Expired));
        }
        match ClaimBridgeWorker::arm(claim_socket).await {
            Ok(worker) => return Ok(RearmOutcome::Rearmed(worker)),
            Err(_) => tokio::time::sleep(CLAIM_STATUS_POLL).await,
        }
    }
}

fn observe_issuance(authorization_hash: &str) -> Result<ClaimIssuanceStatus> {
    let Some(state) = identity_state::load()? else {
        return Ok(ClaimIssuanceStatus::Replaced);
    };
    Ok(state.issuance_status(authorization_hash, chrono::Utc::now().timestamp_millis()))
}

/// Complete one forwarded owner-root call from the daemon-hosted router.
///
/// The daemon relays only the verified principal and the raw request
/// body; parsing, the weft `BeginWebAuthnRegistration` round trip, the
/// owner-root co-sign with the agent signer, and reply encoding all
/// happen here in the foreground process. The daemon never holds the
/// agent signer.
pub(crate) async fn handle_owner_root_body(
    client: &super::hosted::HostedClient,
    method: &str,
    principal: &VerifiedClaimPrincipal,
    body: &[u8],
) -> std::result::Result<Vec<u8>, CallFailure> {
    super::claim_native::handle(client, method, principal, body).map_err(|error| CallFailure {
        code: CallFailureCode::FailedPrecondition as i32,
        message: error.to_string(),
        error: None,
    })
}

fn deactivate_offer(authorization_hash: &str) -> Result<()> {
    let _guard = identity_state::write_lock()?;
    let Some(mut state) = identity_state::load_while_locked()? else {
        return Ok(());
    };
    if state.deactivate_issuance(authorization_hash) {
        identity_state::store_while_locked(&state)?;
    }
    Ok(())
}

fn normalized_web_origin(value: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(value).context("claim web origin must be an absolute URL")?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("claim web origin must be an https origin (scheme, host, and optional port only)");
    }
    Ok(parsed.origin().ascii_serialization())
}

/// Claim-link destination for the local bearer secret.
///
/// Precedence: explicit `--web-origin` (https origin-only, any host), then a
/// server-advertised origin that binds to the configured hosted server
/// (same host or same Public Suffix registrable domain), then
/// [`HEDDLE_SAAS_CLAIM_ORIGIN`] only when that server is `api.heddle.sh`.
fn resolve_web_origin(
    explicit: Option<&str>,
    advertised: Option<&str>,
    server: &str,
) -> Result<String> {
    if let Some(explicit) = explicit {
        return normalized_web_origin(explicit).with_context(
            || "--web-origin must be an https origin (scheme, host, and optional port only)",
        );
    }
    if let Some(advertised) = advertised.map(str::trim).filter(|value| !value.is_empty()) {
        let origin = normalized_web_origin(advertised).with_context(|| {
            "server-advertised claim origin is not a valid https origin; pass --web-origin <https origin> to override"
        })?;
        bind_origin_to_server(&origin, server)?;
        return Ok(origin);
    }
    if is_heddle_saas_api(server)? {
        return normalized_web_origin(HEDDLE_SAAS_CLAIM_ORIGIN);
    }
    bail!(
        "hosted server {server} is not {HEDDLE_SAAS_API} and has no bound claim web origin; pass --web-origin <https origin>"
    )
}

fn is_heddle_saas_api(server: &str) -> Result<bool> {
    Ok(canonical_server_authority(server)? == HEDDLE_SAAS_API)
}

fn bind_origin_to_server(origin: &str, server: &str) -> Result<()> {
    let origin_host = origin_host(origin)?;
    let server_host = hosted_server_host(server)?;
    if origin_host == server_host || same_registrable_domain(&origin_host, &server_host) {
        return Ok(());
    }
    bail!(
        "server-advertised claim origin {origin} is not bound to hosted server {server}; pass --web-origin <https origin> to override"
    )
}

fn origin_host(origin: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(origin).context("claim web origin is not a valid URL")?;
    parsed
        .host_str()
        .map(ascii_host)
        .context("claim web origin has no host")
}

fn hosted_server_host(server: &str) -> Result<String> {
    let canonical = canonical_server_authority(server)?;
    let parsed = reqwest::Url::parse(&canonical)
        .context("configured hosted server is not a valid HTTPS authority")?;
    parsed
        .host_str()
        .map(ascii_host)
        .context("configured hosted server has no host")
}

fn ascii_host(host: &str) -> String {
    host.to_ascii_lowercase()
}

fn same_registrable_domain(left_host: &str, right_host: &str) -> bool {
    match (psl::domain_str(left_host), psl::domain_str(right_host)) {
        (Some(left), Some(right)) => left.as_bytes() == right.as_bytes(),
        _ => false,
    }
}

fn claim_link(web_origin: &str, node_id: &str, secret: &str, relay_url: &str) -> Result<String> {
    let relay = reqwest::Url::parse(relay_url).context("invalid daemon relay URL")?;
    if relay.scheme() != "https"
        || relay.host_str().is_none()
        || !relay.username().is_empty()
        || relay.password().is_some()
        || relay.query().is_some()
        || relay.fragment().is_some()
    {
        bail!("browser claim requires an HTTPS relay URL without credentials, query or fragment");
    }
    let mut link = reqwest::Url::parse(&format!("{web_origin}/claim/{node_id}.{secret}"))?;
    link.query_pairs_mut().append_pair("relay", relay.as_str());
    Ok(link.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_link_uses_the_origin_node_and_bearer_secret() {
        let origin = normalized_web_origin("https://heddle.example:8443/").expect("origin");
        assert_eq!(
            claim_link(
                &origin,
                &"11".repeat(32),
                "short-lived-secret",
                "https://relay.example"
            )
            .expect("claim link"),
            format!(
                "https://heddle.example:8443/claim/{}.short-lived-secret?relay=https%3A%2F%2Frelay.example%2F",
                "11".repeat(32)
            )
        );
    }

    #[test]
    fn claim_link_refuses_a_non_origin_base() {
        for invalid in [
            "heddle.example",
            "ftp://heddle.example",
            "http://heddle.example",
            "http://localhost:3000",
            "https://heddle.example/path",
            "https://heddle.example/?query=1",
            "https://user:pass@heddle.example",
        ] {
            assert!(
                normalized_web_origin(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn explicit_https_origin_wins_even_when_cross_site() {
        assert_eq!(
            resolve_web_origin(
                Some("https://explicit.example:8443/"),
                Some("https://app.acme.example"),
                "api.acme.example",
            )
            .expect("explicit origin"),
            "https://explicit.example:8443"
        );
    }

    #[test]
    fn saas_api_falls_back_to_app_heddle_sh() {
        assert_eq!(
            resolve_web_origin(None, None, "api.heddle.sh").expect("saas default"),
            HEDDLE_SAAS_CLAIM_ORIGIN
        );
        assert_eq!(
            resolve_web_origin(None, None, "https://api.heddle.sh").expect("canonical saas"),
            HEDDLE_SAAS_CLAIM_ORIGIN
        );
        assert_eq!(HEDDLE_SAAS_CLAIM_ORIGIN, "https://app.heddle.sh");
    }

    #[test]
    fn self_hosted_without_a_bound_origin_refuses_the_saas_default() {
        let error = resolve_web_origin(None, None, "api.acme.example")
            .expect_err("self-hosted must not fall back to SaaS");
        let message = error.to_string();
        assert!(
            message.contains("--web-origin"),
            "refusal should name the override: {message}"
        );
        assert!(
            !message.contains(HEDDLE_SAAS_CLAIM_ORIGIN) || message.contains("pass --web-origin"),
            "refusal must not silently select the SaaS app origin: {message}"
        );
    }

    #[test]
    fn bound_server_advertised_origin_is_accepted() {
        assert_eq!(
            resolve_web_origin(None, Some("https://app.acme.example/"), "api.acme.example")
                .expect("same registrable domain"),
            "https://app.acme.example"
        );
        assert_eq!(
            resolve_web_origin(None, Some("https://api.acme.example"), "api.acme.example")
                .expect("same host"),
            "https://api.acme.example"
        );
        assert_eq!(
            resolve_web_origin(None, Some("https://app.acme.co.uk/"), "api.acme.co.uk")
                .expect("psl multi-part suffix"),
            "https://app.acme.co.uk"
        );
    }

    #[test]
    fn leftover_unbound_web_origin_cannot_become_a_claim_link() {
        for advertised in [
            "https://evil.com",
            "http://evil.com",
            "https://evil.com/claim",
            "https://notacme.example",
            "https://bar.github.io",
        ] {
            let error = resolve_web_origin(None, Some(advertised), "api.acme.example")
                .expect_err("unbound leftover origin");
            let message = error.to_string();
            assert!(
                !message.contains("https://evil.com/claim/")
                    && !claim_link_would_use_host(&message, "evil.com"),
                "refused origin must not be formatted as a claim link: {message}"
            );
            assert!(
                message.contains("--web-origin") || message.contains("https"),
                "refusal should be actionable: {message}"
            );
        }
        let error = resolve_web_origin(None, Some("https://bar.github.io"), "api.foo.github.io")
            .expect_err("public suffix is not a string suffix");
        assert!(
            error.to_string().contains("--web-origin"),
            "github.io siblings must not share a registrable domain: {error}"
        );
    }

    #[test]
    fn string_suffix_is_not_a_registrable_domain_bind() {
        let error = resolve_web_origin(None, Some("https://evil.com"), "api.notevil.com")
            .expect_err("ends-with is not a bind");
        assert!(
            error.to_string().contains("--web-origin"),
            "string-suffix trap must fail closed: {error}"
        );
    }

    #[test]
    fn claim_link_refuses_http_even_when_explicit() {
        for invalid in ["http://evil.example", "http://app.heddle.sh"] {
            let error =
                resolve_web_origin(Some(invalid), None, "api.heddle.sh").expect_err("http origin");
            assert!(
                error.to_string().contains("https"),
                "http refusal should name the https requirement: {error}"
            );
        }
    }

    fn claim_link_would_use_host(message: &str, host: &str) -> bool {
        message.contains(&format!("https://{host}/claim/"))
            || message.contains(&format!("http://{host}/claim/"))
    }
}
