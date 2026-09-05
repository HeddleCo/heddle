// SPDX-License-Identifier: Apache-2.0
//! Unix-socket IPC for the policy broker.
//!
//! One framed MessagePack request, one framed reply, then the connection
//! closes. The only value-returning verb is [`BrokerRequest::RunUnwrap`].
//! There is no get-secret RPC. Same-UID sockets are cooperative.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crypto::Signer;
use serde::{Deserialize, Serialize};

use crate::broker::{DecryptPurpose, DecryptRequest, PolicyBroker};
use crate::error::{Result, RuntimeProfileError};

const FRAME_LIMIT: u32 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerRequest {
    /// Authorize and unwrap for a child run. Values return on this verb only.
    RunUnwrap {
        profile: String,
        slots: Vec<String>,
        expires_at_ms: i64,
        caller: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerResponse {
    Unwrapped { slots: Vec<BrokerSlotValue> },
    Denied { code: String, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerSlotValue {
    pub name: String,
    pub value: String,
}

pub fn broker_socket_path(store_root: &Path) -> PathBuf {
    store_root.join("broker.sock")
}

pub fn bind_broker_socket(store_root: &Path) -> Result<UnixListener> {
    let path = broker_socket_path(store_root);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(UnixListener::bind(path)?)
}

pub fn serve_once(listener: &UnixListener, broker: &PolicyBroker, signer: &impl Signer) -> Result<()> {
    let (mut stream, _) = listener.accept()?;
    handle_connection(&mut stream, broker, signer)
}

pub fn handle_connection(
    stream: &mut UnixStream,
    broker: &PolicyBroker,
    signer: &impl Signer,
) -> Result<()> {
    let request = read_request(stream)?;
    let response = dispatch(broker, signer, request);
    write_frame(stream, &encode_response(&response)?)
}

pub fn request_run_unwrap(
    socket: &Path,
    request: &DecryptRequest,
) -> Result<Vec<(String, String)>> {
    let mut stream = UnixStream::connect(socket)?;
    let wire = BrokerRequest::RunUnwrap {
        profile: request.profile.clone(),
        slots: request.slots.clone(),
        expires_at_ms: request.expires_at_ms,
        caller: request.caller.clone(),
    };
    write_frame(&mut stream, &encode_request(&wire)?)?;
    match decode_response(&read_frame(&mut stream)?)? {
        BrokerResponse::Unwrapped { slots } => Ok(slots
            .into_iter()
            .map(|slot| (slot.name, slot.value))
            .collect()),
        BrokerResponse::Denied { message, .. } => Err(RuntimeProfileError::BrokerDenied(message)),
    }
}

fn dispatch(broker: &PolicyBroker, signer: &impl Signer, request: BrokerRequest) -> BrokerResponse {
    match request {
        BrokerRequest::RunUnwrap {
            profile,
            slots,
            expires_at_ms,
            caller,
        } => {
            let now = match crate::store::now_ms() {
                Ok(now) => now,
                Err(err) => {
                    return BrokerResponse::Denied {
                        code: "invalid".to_string(),
                        message: err.to_string(),
                    };
                }
            };
            let request = DecryptRequest {
                profile,
                slots,
                expires_at_ms,
                purpose: DecryptPurpose::Run,
                caller,
            };
            match broker.authorize(&request, now, signer) {
                Ok(grant) => match broker.unwrap_for_run(grant, now, signer) {
                    Ok(secrets) => match secrets.into_env_pairs() {
                        Ok(pairs) => BrokerResponse::Unwrapped {
                            slots: pairs
                                .into_iter()
                                .map(|(name, value)| BrokerSlotValue { name, value })
                                .collect(),
                        },
                        Err(err) => deny(err),
                    },
                    Err(err) => deny(err),
                },
                Err(err) => deny(err),
            }
        }
    }
}

fn deny(err: RuntimeProfileError) -> BrokerResponse {
    let code = match &err {
        RuntimeProfileError::BrokerDenied(message) if message.contains("expired") => "expired",
        RuntimeProfileError::BrokerDenied(_) | RuntimeProfileError::InvalidGrant(_) => {
            "unauthorized"
        }
        RuntimeProfileError::ProfileNotFound(_) | RuntimeProfileError::SlotNotFound(_) => {
            "not_found"
        }
        _ => "denied",
    };
    BrokerResponse::Denied {
        code: code.to_string(),
        message: err.to_string(),
    }
}

fn encode_request(value: &BrokerRequest) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(value)
        .map_err(|err| RuntimeProfileError::Encoding(format!("encode broker request: {err}")))
}

fn encode_response(value: &BrokerResponse) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(value)
        .map_err(|err| RuntimeProfileError::Encoding(format!("encode broker response: {err}")))
}

fn decode_response(bytes: &[u8]) -> Result<BrokerResponse> {
    rmp_serde::from_slice(bytes)
        .map_err(|err| RuntimeProfileError::Decoding(format!("decode broker response: {err}")))
}

fn read_request(stream: &mut UnixStream) -> Result<BrokerRequest> {
    let bytes = read_frame(stream)?;
    rmp_serde::from_slice(&bytes)
        .map_err(|err| RuntimeProfileError::Decoding(format!("decode broker request: {err}")))
}

fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| RuntimeProfileError::Invalid("broker frame too large".to_string()))?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > FRAME_LIMIT {
        return Err(RuntimeProfileError::Invalid(
            "broker frame exceeds limit".to_string(),
        ));
    }
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}
