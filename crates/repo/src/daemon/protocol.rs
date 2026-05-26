// SPDX-License-Identifier: Apache-2.0
//! JSON-over-TCP framing for helper subprocess RPCs.
//!
//! One line in, one line out. Each connection is dedicated to a
//! single request/response pair — same shape the fsmonitor has
//! shipped on for ~6 months, deliberately kept that way so we don't
//! have to reason about half-read messages or framing under restart.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Shutdown, TcpStream},
    time::Duration,
};

use objects::error::HeddleError;
use serde::{Serialize, de::DeserializeOwned};

use super::endpoint::EndpointState;

pub const HELPER_HOST: &str = "127.0.0.1";
pub const HELPER_CONNECT_TIMEOUT_MS: u64 = 1000;
pub const HELPER_IDLE_TIMEOUT_SECS: u64 = 300;
pub const HELPER_IDLE_POLL_MS: u64 = 5;

/// Send a single JSON request to a helper and decode its single-line
/// JSON reply. Used by both the fsmonitor and mount daemon clients.
pub fn send_json_request<Req, Resp>(
    endpoint: &EndpointState,
    request: &Req,
) -> Result<Resp, HeddleError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    let address = format!("{}:{}", endpoint.host, endpoint.port);
    let mut stream = TcpStream::connect_timeout(
        &address
            .parse()
            .map_err(|error| HeddleError::Config(format!("parse helper address: {error}")))?,
        Duration::from_millis(HELPER_CONNECT_TIMEOUT_MS),
    )?;
    stream.set_read_timeout(Some(Duration::from_millis(HELPER_CONNECT_TIMEOUT_MS)))?;
    stream.set_write_timeout(Some(Duration::from_millis(HELPER_CONNECT_TIMEOUT_MS)))?;
    serde_json::to_writer(&mut stream, request)
        .map_err(|error| HeddleError::Config(format!("encode helper request: {error}")))?;
    stream.write_all(b"\n")?;
    stream.shutdown(Shutdown::Write)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line)
        .map_err(|error| HeddleError::Config(format!("decode helper response: {error}")))
}
