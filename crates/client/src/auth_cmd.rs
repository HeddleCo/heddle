//! `heddle auth` command implementations.

use anyhow::{Context, Result, bail};
use crypto::{Ed25519Signer, Signer};
use grpc::heddle::v1::{
    CreateDeviceAuthorizationRequest, CreateServiceAccountRequest, DeviceAuthProof,
    DeviceAuthorizationResponse, ExchangeDeviceAuthorizationRequest,
    IssueServiceAccountCredentialRequest, MintBiscuitRequest, WaitForDeviceAuthorizationRequest,
    auth_service_client::AuthServiceClient, mint_biscuit_request::Proof,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tonic::{
    Request,
    metadata::MetadataValue,
    transport::{Channel, Endpoint},
};
use weft_client_shim::{CliContext, HostedRecoveryAdvice};

use crate::{auth_requests::AuthCommand, credentials, credentials::ServerCredential};

/// Top-level dispatch for `heddle auth <subcommand>`. `_ctx` is
/// reserved for future hosted commands that need repo path / output
/// mode — today's auth subcommands all operate on global credential
/// state and don't read it.
#[derive(Serialize)]
struct AuthLogoutOutput {
    output_kind: &'static str,
    server: String,
    removed: bool,
    /// Whether a device signing identity recorded for this server (heddle#482)
    /// was removed. `false` when none was linked; on a removal failure logout
    /// errors instead of emitting this output, so a `true` here always means
    /// the logged-out private key is no longer on disk.
    device_identity_removed: bool,
}

#[derive(Serialize)]
struct AuthStatusOutput {
    output_kind: &'static str,
    server: String,
    authenticated: bool,
    subject: Option<String>,
    credential_id: Option<String>,
    expires_at: Option<String>,
    recommended_action: Option<String>,
}

#[derive(Serialize)]
struct ServiceTokenOutput {
    output_kind: &'static str,
    name: String,
    namespace: String,
    scope: String,
    token: String,
    /// Absolute path of the private-key PEM file written with mode 0600.
    private_key_path: String,
    /// Only populated when `--show-secrets` is passed; omitted from JSON otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    private_key_pem: Option<String>,
    expires_in_days: u32,
}

const SERVICE_TOKEN_TTL_DAYS: u32 = 30;
const SERVICE_TOKEN_TTL_SECS: i64 = SERVICE_TOKEN_TTL_DAYS as i64 * 24 * 3600;
const ISSUE_SA_PROOF_DOMAIN: &[u8] = b"heddle-sa-credential-issue-v1";
const ISSUE_SA_PROOF_TS_HEADER: &str = "x-heddle-issue-sa-proof-ts";
const ISSUE_SA_PROOF_SIG_HEADER: &str = "x-heddle-issue-sa-proof-sig-bin";

pub async fn cmd_auth(ctx: &dyn CliContext, command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Login { server, no_browser } => cmd_auth_login(&server, no_browser).await,
        AuthCommand::Logout { server } => cmd_auth_logout(ctx, server.as_deref()),
        AuthCommand::Status { server } => cmd_auth_status(ctx, server.as_deref()),
        AuthCommand::CreateServiceToken {
            name,
            namespace,
            server,
            key_out,
            show_secrets,
        } => {
            cmd_create_service_token(
                ctx,
                server.as_deref(),
                name,
                namespace,
                key_out,
                show_secrets,
            )
            .await
        }
    }
}

/// Authenticate via device authorization flow.
async fn cmd_auth_login(server: &str, no_browser: bool) -> Result<()> {
    // 1. Generate Ed25519 keypair for device binding.
    let signer = Ed25519Signer::generate()
        .map_err(|e| anyhow::anyhow!("failed to generate keypair: {e}"))?;
    let public_key_bytes = signer.public_key().to_vec();
    let private_key_pem = signer
        .to_pem()
        .map_err(|e| anyhow::anyhow!("failed to export private key: {e}"))?;

    // 2. Connect to the auth service.
    let mut auth_client: AuthServiceClient<Channel> = connect_auth_client(server).await?;

    // 3. Create device authorization.
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "heddle-cli".to_string());

    let response: DeviceAuthorizationResponse = auth_client
        .create_device_authorization(CreateDeviceAuthorizationRequest {
            device_name: hostname,
            device_public_key: public_key_bytes.clone(),
            scope: "repo:*".to_string(),
            client_operation_id: String::new(),
        })
        .await
        .map_err(|status: tonic::Status| {
            anyhow::anyhow!("create_device_authorization failed: {}", status.message())
        })?
        .into_inner();

    let verification_uri = &response.verification_uri;
    let user_code = &response.user_code;
    let device_code = &response.device_code;

    // 4. Print instructions.
    println!();
    println!("Open this URL to authorize:");
    println!("  {verification_uri}");
    println!();
    println!("Enter code: {user_code}");
    println!();

    // 5. Attempt to open browser. The verification URI is server-controlled,
    // so validate scheme/host (and reject shell metacharacters) before
    // spawning a browser helper — especially on Windows where `cmd /C start`
    // would otherwise interpret the URL.
    if !no_browser {
        let encoded_code = percent_encode_query_component(user_code);
        let url = format!("{verification_uri}?code={encoded_code}");
        match validate_browser_url(&url) {
            Ok(()) => {
                if let Err(_e) = open_url(&url) {
                    eprintln!("Could not open browser automatically. Please open the URL above.");
                }
            }
            Err(err) => {
                eprintln!("Refusing to open browser URL: {err}");
                eprintln!("Please open the URL printed above in your browser.");
            }
        }
    }

    // 6. Poll for approval.
    println!("Waiting for authorization...");

    let access_token = poll_for_approval(
        &mut auth_client,
        device_code,
        &public_key_bytes,
        &signer,
        response.expires_at,
    )
    .await?;

    // 7. Store credential.
    let credential = ServerCredential {
        token: access_token.token,
        subject: access_token.subject.clone(),
        device_id: None,
        credential_id: if access_token.credential_id.is_empty() {
            None
        } else {
            Some(access_token.credential_id)
        },
        private_key_pem: Some(private_key_pem.clone()),
        expires_at: access_token.expires_at.as_ref().and_then(|ts| {
            chrono::DateTime::from_timestamp(ts.seconds, ts.nanos.max(0) as u32)
                .map(|dt| dt.to_rfc3339())
        }),
    };

    credentials::store_server_credential(server, credential)?;

    // Reconcile the local signing identity with the device key (heddle#482):
    // record the device key as the machine's active signing identity so
    // subsequent captures sign with it (it supersedes any per-repo local key;
    // states already signed by a local key keep verifying). Best-effort — a
    // failure here just leaves captures signing with the local key.
    if let Err(error) = repo::identity::link_device_key(&public_key_bytes, &private_key_pem, server)
    {
        tracing::warn!(%error, "could not record device signing identity; captures will use the per-repo local key");
    }

    println!();
    println!(
        "Authenticated as {}. Credentials saved.",
        access_token.subject
    );
    Ok(())
}

/// Remove stored credentials.
fn cmd_auth_logout(ctx: &dyn CliContext, server: Option<&str>) -> Result<()> {
    let server = resolve_server(server)?;

    // Remove the device signing identity `auth login` recorded for THIS server
    // BEFORE dropping the credential (heddle#482 ordering). Fail-closed: if a
    // matching device key is on disk but can't be removed, surface the error
    // rather than reporting a clean logout while the logged-out private key —
    // which `signing_signer()` would keep preferring for every capture — still
    // persists. Unlinking first keeps a failed logout retryable: were the
    // credential removed first, `resolve_server` would no longer resolve back to
    // this server, so a no-arg retry could never re-target the matching
    // `device-identity.toml` still on disk. Holding the credential until the
    // unlink succeeds preserves the same server resolution for the retry.
    let device_identity_removed = repo::identity::unlink_device_key(&server).map_err(|error| {
        anyhow::anyhow!("failed to remove device signing identity for {server}: {error}")
    })?;
    credentials::remove_server_credential(&server)?;

    if ctx.should_output_json(None) {
        let output = AuthLogoutOutput {
            output_kind: "auth_logout",
            server,
            removed: true,
            device_identity_removed,
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Credentials removed for {server}.");
        if device_identity_removed {
            println!("Device signing identity removed.");
        }
    }
    Ok(())
}

/// Show current authentication status.
fn cmd_auth_status(ctx: &dyn CliContext, server: Option<&str>) -> Result<()> {
    let server = resolve_server(server)?;
    match credentials::get_server_credential(&server)? {
        Some(cred) => {
            if ctx.should_output_json(None) {
                let output = AuthStatusOutput {
                    output_kind: "auth_status",
                    server,
                    authenticated: true,
                    subject: Some(cred.subject),
                    credential_id: cred.credential_id,
                    expires_at: cred.expires_at,
                    recommended_action: None,
                };
                println!("{}", serde_json::to_string(&output)?);
            } else {
                println!("Server:        {server}");
                println!("Subject:       {}", cred.subject);
                if let Some(ref cred_id) = cred.credential_id {
                    println!("Credential:    {cred_id}");
                }
                if let Some(ref expires) = cred.expires_at {
                    println!("Expires:       {expires}");
                }
            }
        }
        None => {
            let recommended_action = format!("heddle auth login --server {server}");
            if ctx.should_output_json(None) {
                let output = AuthStatusOutput {
                    output_kind: "auth_status",
                    server,
                    authenticated: false,
                    subject: None,
                    credential_id: None,
                    expires_at: None,
                    recommended_action: Some(recommended_action),
                };
                println!("{}", serde_json::to_string(&output)?);
            } else {
                println!("Not authenticated with {server}.");
                println!("Run `{recommended_action}` to authenticate.");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Create service token
// ---------------------------------------------------------------------------

/// Create a namespace-scoped service token for CI/ephemeral runners.
async fn cmd_create_service_token(
    ctx: &dyn CliContext,
    server: Option<&str>,
    name: String,
    namespace: String,
    key_out: Option<String>,
    show_secrets: bool,
) -> Result<()> {
    let server = resolve_server(server)?;
    let scope = format!("repo:{namespace}/*");

    // Load the calling user's token to authenticate with the server.
    let cred = credentials::get_server_credential(&server)?
        .ok_or_else(|| anyhow::anyhow!(HostedRecoveryAdvice::auth_required(&server)))?;

    // Generate a fresh Ed25519 keypair for the service account credential.
    let signer = Ed25519Signer::generate()
        .map_err(|e| anyhow::anyhow!("failed to generate keypair: {e}"))?;
    let public_key_bytes = signer.public_key().to_vec();
    let private_key_pem = signer
        .to_pem()
        .map_err(|e| anyhow::anyhow!("failed to export service-account private key: {e}"))?;

    // Always persist the private key to a 0600 file; never dump PEM to stdout
    // by default (shell history / CI logs). `--show-secrets` opts into printing
    // the PEM (and including it in JSON).
    let key_path = resolve_service_account_key_path(&name, key_out.as_deref())?;
    if let Some(parent) = key_path.parent() {
        objects::fs_atomic::create_private_dir_all(parent)
            .with_context(|| format!("creating private key directory {}", parent.display()))?;
    }
    objects::fs_atomic::write_file_atomic_secret(&key_path, private_key_pem.as_bytes())
        .with_context(|| format!("writing private key to {}", key_path.display()))?;
    let key_path_display = key_path.display().to_string();

    let channel = connect_channel(&server).await?;

    // Attach the caller's token as a Bearer header on every request.
    let bearer: MetadataValue<_> = format!("Bearer {}", cred.token)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid token format: {e}"))?;

    let mut auth_client =
        AuthServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        });

    // 1. Create the service account.
    let sa_response = auth_client
        .create_service_account(CreateServiceAccountRequest {
            subject: name.clone(),
            display_name: name.clone(),
            scope: scope.clone(),
            client_operation_id: String::new(),
        })
        .await
        .map_err(|status| anyhow::anyhow!("create_service_account failed: {}", status.message()))?
        .into_inner();

    tracing::info!(
        service_account_id = %sa_response.service_account_id,
        subject = %sa_response.subject,
        "service account created"
    );

    // 2. Issue a credential (token) for the service account.
    let credential_request = IssueServiceAccountCredentialRequest {
        service_account_id: sa_response.service_account_id,
        public_key: public_key_bytes,
        scope: scope.clone(),
        // CLI-issued tokens retain their pre-TTL behaviour: 30-day
        // expiry from the server's default (applied when ttl_secs == 0
        // in the handler is "never expires", so pass 30 days here
        // explicitly to preserve prior semantics).
        ttl_secs: Some(prost_types::Duration {
            seconds: SERVICE_TOKEN_TTL_SECS,
            nanos: 0,
        }),
        client_operation_id: String::new(),
    };
    let credential_request = issue_service_account_credential_request(credential_request, &signer)?;
    let issued = auth_client
        .issue_service_account_credential(credential_request)
        .await
        .map_err(|status| {
            anyhow::anyhow!(
                "issue_service_account_credential failed: {}",
                status.message()
            )
        })?
        .into_inner();

    if ctx.should_output_json(None) {
        let output = ServiceTokenOutput {
            output_kind: "auth_create_service_token",
            name,
            namespace,
            scope,
            token: issued.token,
            private_key_path: key_path_display,
            private_key_pem: show_secrets.then_some(private_key_pem),
            expires_in_days: SERVICE_TOKEN_TTL_DAYS,
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!();
        println!("Service token created for \"{}\" (scope: {scope})", name);
        println!();
        println!("Token: {}", issued.token);
        println!();
        println!("Private key written to: {key_path_display}");
        if show_secrets {
            println!();
            println!("Private key PEM:");
            println!("{private_key_pem}");
        }
        println!("This token is proof-of-possession bound to the private key file above.");
        println!("Set the token as HEDDLE_REMOTE_TOKEN in your CI environment.");
        println!(
            "Configure remote.auth_proof_key_pem_path to {key_path_display} (or copy the key securely)."
        );
        println!("This token is scoped to the {namespace} namespace.");
    }

    Ok(())
}

/// Resolve where to write the service-account private key.
///
/// Prefers an explicit `--key-out` path; otherwise writes under
/// `<heddle_home>/service-accounts/<sanitized-name>.pem`.
fn resolve_service_account_key_path(
    name: &str,
    key_out: Option<&str>,
) -> Result<std::path::PathBuf> {
    if let Some(path) = key_out {
        return Ok(std::path::PathBuf::from(path));
    }
    let mut safe: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        safe = "service-account".to_string();
    }
    Ok(repo::identity::heddle_home_dir()
        .join("service-accounts")
        .join(format!("{safe}.pem")))
}

fn issue_service_account_credential_request(
    request: IssueServiceAccountCredentialRequest,
    signer: &Ed25519Signer,
) -> Result<Request<IssueServiceAccountCredentialRequest>> {
    let timestamp = current_unix_timestamp_i64()?;
    issue_service_account_credential_request_at(request, signer, timestamp)
}

fn issue_service_account_credential_request_at(
    request: IssueServiceAccountCredentialRequest,
    signer: &Ed25519Signer,
    timestamp: i64,
) -> Result<Request<IssueServiceAccountCredentialRequest>> {
    let signature = issue_service_account_credential_signature(
        signer,
        timestamp,
        &request.service_account_id,
        &request.public_key,
    )?;
    let mut request = Request::new(request);
    request.metadata_mut().insert(
        ISSUE_SA_PROOF_TS_HEADER,
        timestamp
            .to_string()
            .parse()
            .map_err(|err| anyhow::anyhow!("invalid proof timestamp metadata: {err}"))?,
    );
    request.metadata_mut().insert_bin(
        ISSUE_SA_PROOF_SIG_HEADER,
        MetadataValue::from_bytes(&signature),
    );
    Ok(request)
}

fn issue_service_account_credential_signature(
    signer: &Ed25519Signer,
    timestamp: i64,
    service_account_id: &str,
    public_key: &[u8],
) -> Result<Vec<u8>> {
    let canonical = derive_issue_service_account_credential_canonical(
        timestamp,
        service_account_id,
        public_key,
    );
    signer
        .sign(&canonical)
        .map_err(|e| anyhow::anyhow!("failed to sign service-account proof: {e}"))
}

fn derive_issue_service_account_credential_canonical(
    timestamp: i64,
    service_account_id: &str,
    public_key: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ISSUE_SA_PROOF_DOMAIN);
    hasher.update([0u8]);
    hasher.update(timestamp.to_be_bytes());
    hasher.update([0u8]);
    hasher.update(service_account_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(public_key);
    hasher.finalize().into()
}

fn current_unix_timestamp_i64() -> Result<i64> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| anyhow::anyhow!("system clock is before unix epoch: {err}"))?
        .as_secs();
    i64::try_from(secs).map_err(|_| anyhow::anyhow!("system clock exceeds i64 unix timestamp"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve server from explicit arg, default credential, or fallback.
fn resolve_server(explicit: Option<&str>) -> Result<String> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    if let Some(default) = credentials::default_server()? {
        return Ok(default);
    }
    Ok("grpc.heddle.sh".to_string())
}

/// Connect a raw gRPC channel to the given server.
async fn connect_channel(server: &str) -> Result<Channel> {
    let uri = infer_server_uri(server);
    // F2: the auth-login / service-token connect path sends the device key and
    // receives the bearer biscuit. Refuse cleartext (`http://`) to a
    // non-loopback address unless the operator explicitly opts in, mirroring
    // the remote paths' `cleartext_connect_allowed` gate. Loopback stays free.
    enforce_auth_cleartext_gate(&uri)?;
    let endpoint = Endpoint::from_shared(uri.clone())
        .map_err(|e| anyhow::anyhow!("invalid server address '{server}': {e}"))?;
    endpoint
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to {server}: {e}"))
}

/// Refuse a cleartext (`http://`) auth connection to a non-loopback address
/// unless the operator has opted in via `HEDDLE_REMOTE_INSECURE`.
///
/// This routes the auth-login / service-token connect path through the same
/// `cleartext_connect_allowed` semantics the remote paths use: TLS is always
/// allowed, cleartext to loopback is allowed, cleartext to a non-loopback
/// address is rejected unless the insecure opt-in is set. Fail-closed: an
/// `http://` URI whose host is not a parseable loopback IP literal is treated
/// as non-loopback and refused without the opt-in.
fn enforce_auth_cleartext_gate(uri: &str) -> Result<()> {
    // Only cleartext connections are gated; `https://` is always permitted.
    let Some(rest) = uri.strip_prefix("http://") else {
        return Ok(());
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Drop userinfo if present, then split host[:port].
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        // IPv6 literal: [::1]:port
        inner.split(']').next().unwrap_or(inner)
    } else {
        hostport
            .rsplit_once(':')
            .map(|(host, _port)| host)
            .unwrap_or(hostport)
    };

    // `localhost` resolves to loopback but is not an `IpAddr`; treat it as
    // allowed. Any host that does not parse as a loopback IP literal is
    // fail-closed non-loopback.
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(());
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if cli_shared::is_loopback_ip(ip) {
            return Ok(());
        }
        // Non-loopback cleartext: honor the insecure opt-in and reuse the
        // remote gate's error message verbatim.
        let allow_insecure = auth_cleartext_insecure_opt_in()?;
        let addr = std::net::SocketAddr::new(ip, 0);
        if cli_shared::cleartext_connect_allowed(addr, false, allow_insecure) {
            return Ok(());
        }
        bail!(cli_shared::cleartext_refused_message(addr));
    }

    // Non-IP-literal cleartext host (e.g. `localhost`-alias or bare name that
    // `infer_server_uri` chose `http://` for): fail-closed unless opted in.
    if auth_cleartext_insecure_opt_in()? {
        return Ok(());
    }
    bail!(
        "refusing cleartext connection to non-loopback host {host:?}; \
enable TLS or set HEDDLE_REMOTE_INSECURE=1 for intentional cleartext"
    );
}

/// Whether the operator opted in to non-loopback cleartext for the auth path.
///
/// There is no `--insecure` flag on the auth subcommands, so this honors the
/// same `HEDDLE_REMOTE_INSECURE` environment opt-in the remote paths accept.
fn auth_cleartext_insecure_opt_in() -> Result<bool> {
    match std::env::var("HEDDLE_REMOTE_INSECURE") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" | "" => Ok(false),
            other => bail!(
                "invalid HEDDLE_REMOTE_INSECURE value {other:?}; \
expected one of 1/0, true/false, yes/no, or on/off"
            ),
        },
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(err @ std::env::VarError::NotUnicode(_)) => {
            bail!("failed to read HEDDLE_REMOTE_INSECURE: {err}")
        }
    }
}

/// Connect an unauthenticated `AuthServiceClient` to the given server.
async fn connect_auth_client(server: &str) -> Result<AuthServiceClient<Channel>> {
    Ok(AuthServiceClient::new(connect_channel(server).await?))
}

fn infer_server_uri(server: &str) -> String {
    if server.starts_with("http://") || server.starts_with("https://") {
        return server.to_string();
    }

    let authority = server.split('/').next().unwrap_or(server);
    let host = authority
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
        .map(|(value, _)| value)
        .unwrap_or_else(|| {
            authority
                .rsplit_once(':')
                .map(|(value, _)| value)
                .unwrap_or(authority)
        });

    let use_http = host.contains("localhost")
        || host.parse::<std::net::IpAddr>().is_ok()
        || authority.parse::<std::net::SocketAddr>().is_ok();

    if use_http {
        format!("http://{server}")
    } else {
        format!("https://{server}")
    }
}

/// Poll `MintBiscuit(DeviceAuthProof)` until the device code is approved or
/// the authorization expires.
async fn poll_for_approval(
    client: &mut AuthServiceClient<Channel>,
    device_code: &str,
    public_key: &[u8],
    signer: &Ed25519Signer,
    expires_at: Option<prost_types::Timestamp>,
) -> Result<AccessToken> {
    let proof_bytes = device_authorization_signature(device_code, signer)?;

    let expires_at_secs = expires_at
        .as_ref()
        .map(|t| t.seconds.max(0) as u64)
        .unwrap_or(0);
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(
            expires_at_secs
                .saturating_sub(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                )
                .max(30),
        );

    let mut events = client
        .wait_for_device_authorization(WaitForDeviceAuthorizationRequest {
            device_code: device_code.to_string(),
        })
        .await
        .map_err(|status| {
            anyhow::anyhow!("wait_for_device_authorization failed: {}", status.message())
        })?
        .into_inner();

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!("Authorization timed out. Please try again.");
        }

        let event = tokio::time::timeout(remaining, events.message())
            .await
            .map_err(|_| anyhow::anyhow!("Authorization timed out. Please try again."))?
            .map_err(|status| {
                anyhow::anyhow!("device authorization wait failed: {}", status.message())
            })?;

        match event {
            Some(status) if status.status == "pending" => continue,
            Some(status) if status.status == "approved" => break,
            Some(status) if status.status == "expired" => {
                bail!("Authorization expired before approval. Please try again.");
            }
            Some(status) => bail!("Unexpected device authorization status: {}", status.status),
            None => bail!("Device authorization ended before approval. Please try again."),
        }
    }

    match mint_biscuit_with_device_auth(client, device_code, public_key, proof_bytes.clone()).await
    {
        Ok(token) => Ok(token),
        Err(status) if should_fallback_to_exchange_device_authorization(&status) => {
            tracing::debug!(
                status = %status.message(),
                "falling back to ExchangeDeviceAuthorization for lagging auth server"
            );
            exchange_device_authorization(client, device_code, public_key, proof_bytes).await
        }
        Err(status) => Err(anyhow::anyhow!(
            "device authorization failed: {}",
            status.message()
        )),
    }
}

async fn mint_biscuit_with_device_auth(
    client: &mut AuthServiceClient<Channel>,
    device_code: &str,
    public_key: &[u8],
    signature: Vec<u8>,
) -> std::result::Result<AccessToken, tonic::Status> {
    let inner = client
        .mint_biscuit(device_auth_mint_biscuit_request(
            device_code,
            public_key,
            signature,
        ))
        .await?
        .into_inner();

    Ok(AccessToken {
        token: inner.token,
        subject: inner.subject,
        expires_at: inner.expires_at,
        credential_id: inner.credential_id,
    })
}

async fn exchange_device_authorization(
    client: &mut AuthServiceClient<Channel>,
    device_code: &str,
    public_key: &[u8],
    proof: Vec<u8>,
) -> Result<AccessToken> {
    let inner = client
        .exchange_device_authorization(ExchangeDeviceAuthorizationRequest {
            device_code: device_code.to_string(),
            device_public_key: public_key.to_vec(),
            proof,
        })
        .await
        .map_err(|status| anyhow::anyhow!("device authorization failed: {}", status.message()))?
        .into_inner();

    Ok(AccessToken {
        token: inner.token,
        subject: inner.subject,
        expires_at: inner.expires_at,
        credential_id: inner.credential_id,
    })
}

fn device_auth_mint_biscuit_request(
    device_code: &str,
    public_key: &[u8],
    signature: Vec<u8>,
) -> MintBiscuitRequest {
    MintBiscuitRequest {
        subject: String::new(),
        requested_scope: String::new(),
        user_agent: String::new(),
        ip: String::new(),
        proof: Some(Proof::DeviceAuth(DeviceAuthProof {
            device_code: device_code.to_string(),
            device_public_key: public_key.to_vec(),
            signature,
        })),
        client_operation_id: String::new(),
    }
}

fn device_authorization_signature(device_code: &str, signer: &Ed25519Signer) -> Result<Vec<u8>> {
    signer
        .sign(format!("device:{device_code}").as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to sign proof: {e}"))
}

fn should_fallback_to_exchange_device_authorization(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::Unimplemented
        && status.message().contains("DeviceAuthProof")
        && status.message().contains("ExchangeDeviceAuthorization")
}

/// Extracted token fields from `AccessTokenResponse`.
struct AccessToken {
    token: String,
    subject: String,
    expires_at: Option<prost_types::Timestamp>,
    credential_id: String,
}

/// Validate a URL before handing it to a browser helper.
///
/// Accepts only `https://` URLs, or `http://` when the host is loopback
/// (`localhost`, `127.0.0.1`, `::1`). Rejects empty strings, control
/// characters, and shell metacharacters that are unsafe for Windows
/// `cmd /C start` even when passed as separate argv elements (including `%`
/// env-var expansion and `<`/`>` redirection).
///
/// Validation is the primary control; Windows still uses the safer
/// `start "" <url>` form after this check passes.
pub(crate) fn validate_browser_url(url: &str) -> Result<()> {
    if url.is_empty() {
        bail!("browser URL is empty");
    }
    for ch in url.chars() {
        // Fail-closed: reject control chars and every shell/`cmd` metacharacter
        // unsafe for `cmd /C start`. `%` enables env-var expansion (`%VAR%`),
        // and `<`/`>` enable redirection — a hostile auth server could use any
        // of these to inject via the Windows browser launcher.
        if ch.is_control()
            || matches!(
                ch,
                '"' | '\'' | '|' | '&' | '^' | '`' | '%' | '<' | '>' | ' ' | '\n' | '\r' | '\t'
            )
        {
            bail!("browser URL contains forbidden character {ch:?}");
        }
    }

    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("browser URL must include a scheme (https://…)");
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        bail!("browser URL scheme must be https (or http for localhost only)");
    }
    if rest.is_empty() {
        bail!("browser URL is missing a host");
    }

    // Authority ends at the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        bail!("browser URL is missing a host");
    }
    // Drop userinfo if present.
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = extract_url_host(hostport);
    if host.is_empty() {
        bail!("browser URL is missing a host");
    }
    // Spaces already rejected above; also refuse empty host labels.
    if host.chars().any(|ch| ch.is_whitespace()) {
        bail!("browser URL host must not contain whitespace");
    }

    if scheme == "http" && !is_loopback_browser_host(host) {
        bail!("http browser URLs are only allowed for localhost/127.0.0.1/::1");
    }
    Ok(())
}

fn extract_url_host(hostport: &str) -> &str {
    if let Some(inner) = hostport.strip_prefix('[') {
        // IPv6 literal: [::1]:port
        return inner.split(']').next().unwrap_or(inner);
    }
    hostport
        .rsplit_once(':')
        .map(|(host, _port)| host)
        .unwrap_or(hostport)
}

fn is_loopback_browser_host(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Percent-encode a query component using the unreserved set (RFC 3986).
fn percent_encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Best-effort browser open. Caller MUST pass a URL that already passed
/// [`validate_browser_url`]. On Windows, validation is the primary control
/// against command injection via `cmd /C start`.
fn open_url(url: &str) -> Result<()> {
    // Defense in depth: refuse to open unvalidated URLs even if a caller
    // forgets the pre-check.
    validate_browser_url(url)?;

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        // Empty title argument prevents `start` from treating a quoted URL
        // as a window title. Only invoked after validate_browser_url.
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_browser_url_accepts_https() {
        validate_browser_url("https://auth.heddle.sh/device").expect("https ok");
        validate_browser_url("https://auth.heddle.sh/device?code=ABCD-1234").expect("https+query");
    }

    #[test]
    fn validate_browser_url_accepts_loopback_http() {
        validate_browser_url("http://127.0.0.1:8421/path").expect("loopback http");
        validate_browser_url("http://localhost:8421/device").expect("localhost http");
        validate_browser_url("http://[::1]:8421/path").expect("ipv6 loopback http");
    }

    #[test]
    fn validate_browser_url_rejects_injection_and_dangerous_schemes() {
        assert!(
            validate_browser_url("https://x.com & calc").is_err(),
            "shell metacharacters must be rejected"
        );
        assert!(validate_browser_url("file:///etc/passwd").is_err());
        assert!(validate_browser_url("javascript:alert(1)").is_err());
        assert!(validate_browser_url("").is_err());
        assert!(validate_browser_url("http://example.com/device").is_err());
        assert!(validate_browser_url("https://evil.com\"&calc").is_err());
    }

    #[test]
    fn validate_browser_url_rejects_percent_and_redirection() {
        // `%` enables Windows env-var expansion (`%VAR%`) via `cmd /C start`.
        assert!(
            validate_browser_url("https://evil.com/%USERPROFILE%").is_err(),
            "percent (env-var expansion) must be rejected"
        );
        // `<` / `>` enable redirection.
        assert!(
            validate_browser_url("https://evil.com/a<b").is_err(),
            "< (redirection) must be rejected"
        );
        assert!(
            validate_browser_url("https://evil.com/a>b").is_err(),
            "> (redirection) must be rejected"
        );
        // A crafted device/auth URL combining them must not slip through.
        assert!(validate_browser_url("https://evil.com/?x=%TEMP%>out").is_err());
    }

    /// Serializes tests that mutate `HEDDLE_REMOTE_INSECURE`.
    static INSECURE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_insecure_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = INSECURE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HEDDLE_REMOTE_INSECURE").ok();
        match value {
            Some(v) => unsafe { std::env::set_var("HEDDLE_REMOTE_INSECURE", v) },
            None => unsafe { std::env::remove_var("HEDDLE_REMOTE_INSECURE") },
        }
        let out = f();
        match prev {
            Some(v) => unsafe { std::env::set_var("HEDDLE_REMOTE_INSECURE", v) },
            None => unsafe { std::env::remove_var("HEDDLE_REMOTE_INSECURE") },
        }
        out
    }

    #[test]
    fn cleartext_gate_allows_https_and_loopback() {
        with_insecure_env(None, || {
            enforce_auth_cleartext_gate("https://grpc.heddle.sh").expect("https always allowed");
            enforce_auth_cleartext_gate("http://127.0.0.1:8421").expect("loopback v4 allowed");
            enforce_auth_cleartext_gate("http://[::1]:8421").expect("loopback v6 allowed");
            enforce_auth_cleartext_gate("http://localhost:8421").expect("localhost allowed");
        });
    }

    #[test]
    fn cleartext_gate_rejects_nonloopback_without_insecure() {
        with_insecure_env(None, || {
            let err = enforce_auth_cleartext_gate("http://192.168.1.44:8421")
                .expect_err("non-loopback cleartext must be refused");
            let msg = err.to_string();
            assert!(
                msg.contains("refusing cleartext connection to non-loopback"),
                "unexpected message: {msg}"
            );
            // Fail-closed: a bare non-loopback host that inferred http:// is also
            // refused.
            assert!(enforce_auth_cleartext_gate("http://server.internal:8421").is_err());
        });
    }

    #[test]
    fn cleartext_gate_allows_nonloopback_with_insecure_opt_in() {
        with_insecure_env(Some("1"), || {
            enforce_auth_cleartext_gate("http://192.168.1.44:8421")
                .expect("insecure opt-in permits non-loopback cleartext");
        });
    }

    #[test]
    fn cleartext_gate_rejects_invalid_insecure_value() {
        with_insecure_env(Some("maybe"), || {
            assert!(
                enforce_auth_cleartext_gate("http://192.168.1.44:8421").is_err(),
                "an ambiguous opt-in value must fail closed"
            );
        });
    }

    #[test]
    fn percent_encode_query_component_encodes_reserved() {
        assert_eq!(percent_encode_query_component("ABCD-1234"), "ABCD-1234");
        assert_eq!(percent_encode_query_component("a b"), "a%20b");
        assert_eq!(percent_encode_query_component("x&y"), "x%26y");
    }

    #[test]
    fn infers_http_for_plain_ip_targets() {
        assert_eq!(
            infer_server_uri("192.168.1.44:8421"),
            "http://192.168.1.44:8421"
        );
        assert_eq!(infer_server_uri("10.0.0.8"), "http://10.0.0.8");
    }

    #[test]
    fn infers_http_for_loopback_targets() {
        assert_eq!(infer_server_uri("localhost:8421"), "http://localhost:8421");
        assert_eq!(infer_server_uri("[::1]:8421"), "http://[::1]:8421");
    }

    #[test]
    fn keeps_https_default_for_hostnames() {
        assert_eq!(infer_server_uri("grpc.heddle.sh"), "https://grpc.heddle.sh");
        assert_eq!(
            infer_server_uri("example.internal:8443"),
            "https://example.internal:8443"
        );
    }

    #[test]
    fn preserves_explicit_scheme() {
        assert_eq!(
            infer_server_uri("http://example.internal:8421"),
            "http://example.internal:8421"
        );
        assert_eq!(
            infer_server_uri("https://grpc.heddle.sh"),
            "https://grpc.heddle.sh"
        );
    }

    #[test]
    fn device_auth_mint_request_uses_device_auth_proof_variant() {
        let request =
            device_auth_mint_biscuit_request("device-123", &[1, 2, 3, 4], vec![5, 6, 7, 8]);

        assert!(request.subject.is_empty());
        assert!(request.requested_scope.is_empty());
        match request.proof.expect("proof variant") {
            Proof::DeviceAuth(proof) => {
                assert_eq!(proof.device_code, "device-123");
                assert_eq!(proof.device_public_key, vec![1, 2, 3, 4]);
                assert_eq!(proof.signature, vec![5, 6, 7, 8]);
            }
            Proof::Keypair(_) => panic!("device login must use DeviceAuthProof"),
        }
    }

    #[test]
    fn device_authorization_signature_signs_device_code_challenge() {
        let signer = Ed25519Signer::generate().expect("signer");
        let signature =
            device_authorization_signature("device-123", &signer).expect("device proof");

        Ed25519Signer::verify_with_public_key(
            b"device:device-123",
            signer.public_key(),
            &signature,
        )
        .expect("signature must verify against device challenge");
        assert!(
            Ed25519Signer::verify_with_public_key(
                b"device:other",
                signer.public_key(),
                &signature,
            )
            .is_err(),
            "signature must commit to the device code",
        );
    }

    #[test]
    fn issue_service_account_request_attaches_pop_metadata() {
        let signer = Ed25519Signer::generate().expect("signer");
        let public_key = signer.public_key().to_vec();
        let request = IssueServiceAccountCredentialRequest {
            service_account_id: "sa-123".to_string(),
            public_key: public_key.clone(),
            scope: "repo:heddle/platform/*".to_string(),
            ttl_secs: Some(prost_types::Duration {
                seconds: SERVICE_TOKEN_TTL_SECS,
                nanos: 0,
            }),
            client_operation_id: "op-1".to_string(),
        };

        let timestamp = 1_700_000_000;
        let request = issue_service_account_credential_request_at(request, &signer, timestamp)
            .expect("request with proof");

        assert_eq!(
            request
                .metadata()
                .get(ISSUE_SA_PROOF_TS_HEADER)
                .expect("proof timestamp")
                .to_str()
                .expect("ascii timestamp"),
            timestamp.to_string(),
        );
        let signature = request
            .metadata()
            .get_bin(ISSUE_SA_PROOF_SIG_HEADER)
            .expect("proof signature")
            .to_bytes()
            .expect("binary signature");
        let canonical =
            derive_issue_service_account_credential_canonical(timestamp, "sa-123", &public_key);
        Ed25519Signer::verify_with_public_key(&canonical, &public_key, signature.as_ref())
            .expect("proof must be signed by the new service-account key");

        let body = request.get_ref();
        assert_eq!(body.service_account_id, "sa-123");
        assert_eq!(body.public_key, public_key);
        assert_eq!(body.scope, "repo:heddle/platform/*");
    }

    #[test]
    fn issue_service_account_canonical_commits_to_each_field() {
        let public_key = vec![0xAA; 32];
        let base =
            derive_issue_service_account_credential_canonical(1_700_000_000, "sa-1", &public_key);

        assert_ne!(
            base,
            derive_issue_service_account_credential_canonical(1_700_000_001, "sa-1", &public_key,),
        );
        assert_ne!(
            base,
            derive_issue_service_account_credential_canonical(1_700_000_000, "sa-2", &public_key,),
        );
        assert_ne!(
            base,
            derive_issue_service_account_credential_canonical(1_700_000_000, "sa-1", &[0xBB; 32],),
        );
    }

    #[test]
    fn device_auth_fallback_is_limited_to_lagging_weft_stub() {
        let lagging = tonic::Status::unimplemented(
            "MintBiscuit DeviceAuthProof is not implemented yet; use ExchangeDeviceAuthorization for now",
        );
        assert!(should_fallback_to_exchange_device_authorization(&lagging));

        let unrelated = tonic::Status::unimplemented("some other endpoint is missing");
        assert!(!should_fallback_to_exchange_device_authorization(
            &unrelated
        ));

        let denied = tonic::Status::permission_denied(
            "MintBiscuit DeviceAuthProof signature verification failed",
        );
        assert!(!should_fallback_to_exchange_device_authorization(&denied));
    }

    /// Minimal `CliContext` for the logout tests — text output, no repo.
    struct TextCtx;

    impl CliContext for TextCtx {
        fn repo_path(&self) -> Option<&std::path::Path> {
            None
        }
        fn operation_id_wire(&self) -> String {
            String::new()
        }
        fn should_output_json(&self, _repo_config: Option<&repo::Config>) -> bool {
            false
        }
    }

    /// Run `f` with `HOME` pointed at a fresh temp dir and `HEDDLE_HOME` cleared,
    /// so the credential store and device identity both resolve under
    /// `<temp>/.heddle`. Serialised with the credential store's env tests.
    fn with_isolated_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = credentials::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp home");
        let prev_home = std::env::var_os("HOME");
        let prev_heddle_home = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::remove_var("HEDDLE_HOME");
        }
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prev_heddle_home {
                Some(value) => std::env::set_var("HEDDLE_HOME", value),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
        }
        drop(temp);
        match out {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn sample_credential() -> ServerCredential {
        ServerCredential {
            token: "tkn".to_string(),
            subject: "dev".to_string(),
            device_id: None,
            credential_id: None,
            private_key_pem: None,
            expires_at: None,
        }
    }

    /// On a successful logout, both the credential and the matching device
    /// signing identity are removed (heddle#482).
    #[test]
    fn logout_removes_credential_and_device_identity_on_success() {
        with_isolated_home(|| {
            credentials::store_server_credential("grpc.S", sample_credential())
                .expect("store credential");

            // Record a matching device identity via the real link path.
            let signer = Ed25519Signer::generate().expect("keypair");
            repo::identity::link_device_key(
                signer.public_key(),
                &signer.to_pem().expect("pem"),
                "grpc.S",
            )
            .expect("link device key");
            assert!(repo::identity::device_identity_path().exists());

            // No explicit --server: resolve_server falls back to the stored
            // default written by `store_server_credential`.
            cmd_auth_logout(&TextCtx, None).expect("logout succeeds");

            assert!(
                credentials::get_server_credential("grpc.S")
                    .expect("load")
                    .is_none(),
                "credential must be removed on a successful logout",
            );
            assert!(
                !repo::identity::device_identity_path().exists(),
                "device identity must be removed on a successful logout",
            );
        });
    }

    /// Ordering (heddle#482): logout unlinks the device identity BEFORE dropping
    /// the credential, so a fail-closed unlink leaves the credential/default
    /// intact and `heddle auth logout` stays retryable with the SAME server
    /// resolution. A corrupt device-identity file stands in for any unlink
    /// failure (it fails the parse). Were the credential removed first, a no-arg
    /// retry could no longer resolve back to this server.
    #[test]
    fn logout_preserves_credential_when_device_unlink_fails() {
        with_isolated_home(|| {
            credentials::store_server_credential("grpc.S", sample_credential())
                .expect("store credential");

            let device_path = repo::identity::device_identity_path();
            std::fs::create_dir_all(device_path.parent().expect("home parent")).expect("home dir");
            std::fs::write(
                &device_path,
                b"!!! definitely not valid device-identity toml !!!",
            )
            .expect("write corrupt device identity");

            let result = cmd_auth_logout(&TextCtx, None);
            assert!(
                result.is_err(),
                "a failed device unlink must fail the logout, not report a clean removal",
            );

            assert!(
                credentials::get_server_credential("grpc.S")
                    .expect("load")
                    .is_some(),
                "credential must be preserved when unlink fails, so logout is retryable",
            );
            assert_eq!(
                credentials::default_server().expect("default").as_deref(),
                Some("grpc.S"),
                "default server must be preserved so a no-arg retry re-targets the same server",
            );
        });
    }
}
