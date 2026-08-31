// SPDX-License-Identifier: Apache-2.0
//! Cross-process bridge for the owner-root co-sign step of the browser
//! claim ceremony (heddle#1620, decision D3).
//!
//! The persistent box network daemon (`heddle netd`) hosts the
//! `heddle-claim/1` router (see [`mount_claim_router`]). It drives the
//! browser's `Resolve` + `preConsent` + `promoteConsent` calls itself,
//! but it deliberately **does not hold the agent owner-root signer**.
//! When a browser reaches the owner-root co-sign (`ClaimOwnerRoot`), the
//! daemon forwards the call — subject, authorization hash, and the raw
//! request body — over a same-uid Unix socket to a foreground
//! `heddle claim` process, which holds `claim_proof_signer()` and a live
//! [`HostedClient`], completes the co-sign, and returns the reply the
//! daemon relays back to the browser verbatim.
//!
//! The in-process `mpsc` between the router handler and the daemon stays
//! inside the daemon; only the serialized request/reply cross the socket,
//! on a connection the foreground worker holds open for the ceremony
//! window. This keeps the mpsc's oneshot responder daemon-local (it can
//! never cross the socket) while the signer stays foreground-only.

use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::{CallFailure, CallFailureCode};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{Endpoint, protocol::Router};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

use super::{
    claim_authorization::{ClaimOwnerRootCall, StoredClaimAuthorization},
    claim_offer::handle_owner_root_body,
    hosted::{
        HostedClient,
        claim_protocol::{CLAIM_ALPN_V1, ClaimProtocol, VerifiedClaimPrincipal},
    },
};

/// Ceiling on a single bridge frame. Owner-root bodies carry a handful of
/// small protobuf messages; a few hundred KiB is comfortably above the
/// real payloads and well below anything that could pressure memory.
const MAX_BRIDGE_FRAME: usize = 1024 * 1024;

/// Serialized daemon→worker request: the verified principal plus the raw
/// `ClaimOwnerRoot` body. The daemon never interprets the body — parsing,
/// signing, and reply encoding all happen in the foreground worker.
#[derive(Serialize, Deserialize)]
struct BridgeRequest {
    subject: String,
    authorization_hash: String,
    body_b64: String,
}

/// Serialized worker→daemon reply: either the exact reply bytes the
/// browser consumes, or a structured failure to relay back.
#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BridgeReply {
    Ok { reply_b64: String },
    Err { code: i32, message: String },
}

fn failure(code: CallFailureCode, message: impl Into<String>) -> CallFailure {
    CallFailure {
        code: code as i32,
        message: message.into(),
        error: None,
    }
}

/// Box-scoped path of the owner-root co-sign bridge socket:
/// `<heddle_home>/state/heddle-netd-claim.sock`.
///
/// The single source of truth for both the daemon (which binds it) and a
/// foreground `heddle claim` (which connects to it).
pub fn claim_bridge_socket_path(heddle_home: &Path) -> PathBuf {
    repo::daemon::box_state_dir_in(heddle_home).join("heddle-netd-claim.sock")
}

/// The claim router mounted on the daemon's persistent endpoint, plus the
/// channel of owner-root calls the daemon must forward to a foreground
/// signer. Held for the daemon's lifetime; dropping it (or completing
/// [`Self::serve_owner_root_bridge`]) shuts the router down.
pub struct DaemonClaimRouter {
    router: Router,
    owner_root_calls: tokio::sync::mpsc::Receiver<ClaimOwnerRootCall>,
}

/// Mount the `heddle-claim/1` router on a live daemon endpoint.
///
/// The router serves `Resolve` / `preConsent` / `promoteConsent` inline
/// against the file-backed, lock-serialized claim state (so it works
/// across daemon restarts on the persisted node id), and routes the
/// owner-root co-sign out through [`DaemonClaimRouter::serve_owner_root_bridge`].
#[must_use]
pub fn mount_claim_router(endpoint: Endpoint) -> DaemonClaimRouter {
    let (authorization, _completion, owner_root_calls) = StoredClaimAuthorization::new();
    let authorization = std::sync::Arc::new(authorization);
    let router = Router::builder(endpoint)
        .accept(
            CLAIM_ALPN_V1,
            ClaimProtocol::new(std::sync::Arc::clone(&authorization), authorization),
        )
        .spawn();
    DaemonClaimRouter {
        router,
        owner_root_calls,
    }
}

impl DaemonClaimRouter {
    /// Serve the owner-root co-sign bridge on `socket_path` until the
    /// router's endpoint closes (its owner-root sender drops).
    ///
    /// A single foreground worker is armed at a time; a newer connection
    /// replaces an older one. When no worker is armed, or the armed
    /// worker's connection has failed, an owner-root call fails closed
    /// with a `FailedPrecondition` the browser can surface — never a
    /// hang, and never a co-sign the daemon performed itself.
    pub async fn serve_owner_root_bridge(mut self, socket_path: PathBuf) -> Result<()> {
        let listener = bind_bridge_listener(&socket_path)
            .with_context(|| format!("binding claim co-sign bridge socket {}", socket_path.display()))?;
        let mut worker: Option<UnixStream> = None;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) if peer_is_same_uid(&stream) => {
                            // A newer foreground `heddle claim` takes over the
                            // co-sign role from any earlier one.
                            worker = Some(stream);
                        }
                        Ok((stream, _)) => {
                            tracing::warn!("rejecting claim co-sign worker: peer uid mismatch");
                            drop(stream);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "claim co-sign bridge accept failed");
                        }
                    }
                }
                call = self.owner_root_calls.recv() => {
                    let Some(call) = call else {
                        // The router endpoint closed; stop serving.
                        break;
                    };
                    let bridge = OwnerRootBridgeCall::new(call);
                    match worker.as_mut() {
                        Some(stream) => match exchange(stream, bridge.request_bytes()).await {
                            Ok(reply) => bridge.respond(&reply),
                            Err(error) => {
                                tracing::warn!(%error, "claim co-sign worker exchange failed");
                                worker = None;
                                bridge.respond_unavailable();
                            }
                        },
                        None => bridge.respond_unavailable(),
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&socket_path);
        if let Err(error) = self.router.shutdown().await {
            tracing::warn!(%error, "claim router shutdown failed");
        }
        Ok(())
    }
}

/// One owner-root call captured from the router, pre-serialized for the
/// worker. Owns the daemon-local oneshot responder; the reply the worker
/// returns (or its absence) is relayed back through it.
struct OwnerRootBridgeCall {
    call: ClaimOwnerRootCall,
    request: Vec<u8>,
}

impl OwnerRootBridgeCall {
    fn new(call: ClaimOwnerRootCall) -> Self {
        let request = serde_json::to_vec(&BridgeRequest {
            subject: call.principal().subject.clone(),
            authorization_hash: call.principal().authorization_hash.clone(),
            body_b64: URL_SAFE_NO_PAD.encode(call.body()),
        })
        .unwrap_or_default();
        Self { call, request }
    }

    fn request_bytes(&self) -> &[u8] {
        &self.request
    }

    fn respond(self, reply_frame: &[u8]) {
        let response = match serde_json::from_slice::<BridgeReply>(reply_frame) {
            Ok(BridgeReply::Ok { reply_b64 }) => match URL_SAFE_NO_PAD.decode(reply_b64) {
                Ok(reply) => Ok(reply),
                Err(_) => Err(failure(
                    CallFailureCode::Internal,
                    "claim co-sign worker returned a malformed reply",
                )),
            },
            Ok(BridgeReply::Err { code, message }) => Err(CallFailure {
                code,
                message,
                error: None,
            }),
            Err(_) => Err(failure(
                CallFailureCode::Internal,
                "claim co-sign worker returned an unparseable reply",
            )),
        };
        self.call.respond(response);
    }

    fn respond_unavailable(self) {
        self.call.respond(Err(failure(
            CallFailureCode::FailedPrecondition,
            "no foreground `heddle claim` process is armed to co-sign the owner root",
        )));
    }
}

/// A foreground worker's held-open connection to the daemon's co-sign
/// bridge. The worker reads one owner-root request at a time, co-signs it
/// with its local signer, and writes the reply.
pub(crate) struct ClaimBridgeWorker {
    stream: UnixStream,
}

impl ClaimBridgeWorker {
    /// Connect to the daemon's co-sign bridge socket and arm as the
    /// foreground signer for the claim window.
    pub(crate) async fn arm(socket_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).await.with_context(|| {
            format!(
                "connecting to the claim co-sign bridge at {}; is `heddle netd serve` running?",
                socket_path.display()
            )
        })?;
        Ok(Self { stream })
    }

    /// Await the next owner-root request the daemon forwards, co-sign it
    /// with `client`, and return the reply. Returns `Ok(false)` when the
    /// daemon closed the bridge (window over or daemon stopped).
    pub(crate) async fn serve_next(&mut self, client: &HostedClient) -> Result<bool> {
        let Some(request) = read_frame(&mut self.stream).await? else {
            return Ok(false);
        };
        let reply = cosign_owner_root_request(&request, client).await;
        write_frame(&mut self.stream, &reply).await?;
        Ok(true)
    }

    /// Await one forwarded owner-root request and answer it with a
    /// caller-supplied reply, decoding the forwarded principal and body.
    /// Test-only stand-in for [`Self::serve_next`] that does not need a
    /// live [`HostedClient`].
    #[cfg(test)]
    pub(crate) async fn serve_next_canned<F>(&mut self, respond: F) -> Result<bool>
    where
        F: FnOnce(&str, &str, &[u8]) -> std::result::Result<Vec<u8>, CallFailure>,
    {
        let Some(request) = read_frame(&mut self.stream).await? else {
            return Ok(false);
        };
        let request: BridgeRequest =
            serde_json::from_slice(&request).context("decoding forwarded owner-root request")?;
        let body = URL_SAFE_NO_PAD
            .decode(&request.body_b64)
            .context("decoding forwarded owner-root body")?;
        let reply = match respond(&request.subject, &request.authorization_hash, &body) {
            Ok(reply) => BridgeReply::Ok {
                reply_b64: URL_SAFE_NO_PAD.encode(reply),
            },
            Err(failure) => BridgeReply::Err {
                code: failure.code,
                message: failure.message,
            },
        };
        write_frame(&mut self.stream, &serde_json::to_vec(&reply)?).await?;
        Ok(true)
    }
}

/// Foreground co-sign: parse a forwarded request, run the owner-root
/// exchange with the local signer + `client`, and serialize the reply.
///
/// This is the only place the agent owner-root signer is used, and it is
/// only ever reached in the foreground `heddle claim` process — never in
/// the daemon.
async fn cosign_owner_root_request(request_frame: &[u8], client: &HostedClient) -> Vec<u8> {
    let reply = match serde_json::from_slice::<BridgeRequest>(request_frame) {
        Ok(request) => match URL_SAFE_NO_PAD.decode(&request.body_b64) {
            Ok(body) => {
                let principal = VerifiedClaimPrincipal {
                    subject: request.subject,
                    authorization_hash: request.authorization_hash,
                };
                match handle_owner_root_body(client, &principal, &body).await {
                    Ok(reply) => BridgeReply::Ok {
                        reply_b64: URL_SAFE_NO_PAD.encode(reply),
                    },
                    Err(failure) => BridgeReply::Err {
                        code: failure.code,
                        message: failure.message,
                    },
                }
            }
            Err(_) => BridgeReply::Err {
                code: CallFailureCode::Internal as i32,
                message: "claim co-sign request body was malformed".to_string(),
            },
        },
        Err(_) => BridgeReply::Err {
            code: CallFailureCode::Internal as i32,
            message: "claim co-sign request was unparseable".to_string(),
        },
    };
    serde_json::to_vec(&reply).unwrap_or_default()
}

/// Bind the co-sign bridge socket, reusing the mount daemon's mode-0600,
/// fail-closed, same-uid binder so a live worker socket is never stolen.
fn bind_bridge_listener(socket_path: &Path) -> Result<UnixListener> {
    let listener = repo::daemon::bind_unix_socket(socket_path)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    listener
        .set_nonblocking(true)
        .context("marking claim bridge socket non-blocking")?;
    UnixListener::from_std(listener).context("adopting claim bridge socket into the async runtime")
}

fn peer_is_same_uid(stream: &UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(peer) => peer.uid() == unsafe { libc::getuid() },
        Err(error) => {
            tracing::warn!(%error, "could not read claim co-sign peer credentials");
            false
        }
    }
}

/// Daemon side of one request/reply: write the request frame, read the
/// reply frame. A closed connection surfaces as an error so the caller
/// can fail the call closed and disarm the worker.
async fn exchange(stream: &mut UnixStream, request: &[u8]) -> Result<Vec<u8>> {
    write_frame(stream, request).await?;
    read_frame(stream)
        .await?
        .context("claim co-sign worker closed the connection before replying")
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len()).context("claim bridge frame is too large")?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    match stream.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("reading claim bridge frame length"),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_BRIDGE_FRAME {
        anyhow::bail!("claim bridge frame of {length} bytes exceeds the {MAX_BRIDGE_FRAME} limit");
    }
    let mut payload = vec![0u8; length];
    stream
        .read_exact(&mut payload)
        .await
        .context("reading claim bridge frame body")?;
    Ok(Some(payload))
}
