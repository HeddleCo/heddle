// SPDX-License-Identifier: Apache-2.0
//! JSON-over-TCP / JSON-over-UDS framing for helper subprocess RPCs.
//!
//! One line in, one line out. Each connection is dedicated to a
//! single request/response pair — same shape the fsmonitor has
//! shipped on for ~6 months, deliberately kept that way so we don't
//! have to reason about half-read messages or framing under restart.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Shutdown, TcpStream},
    path::Path,
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
/// JSON reply. Used by the fsmonitor TCP helper.
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
    write_json_line(&mut stream, request)?;
    stream.shutdown(Shutdown::Write)?;
    read_json_line(stream)
}

/// Mount-daemon RPC. Refuses localhost TCP: only a same-uid UDS path
/// is accepted (heddle#901).
pub fn send_mount_daemon_request<Req, Resp>(
    endpoint: &EndpointState,
    request: &Req,
) -> Result<Resp, HeddleError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    let Some(socket_path) = endpoint.socket_path.as_ref() else {
        return Err(HeddleError::Config(
            "mount daemon endpoint has no authenticated socket; refusing localhost TCP".into(),
        ));
    };
    send_json_request_unix(socket_path, request)
}

/// Send one JSON line over a Unix-domain socket and read the reply.
pub fn send_json_request_unix<Req, Resp>(
    socket_path: &Path,
    request: &Req,
) -> Result<Resp, HeddleError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(Duration::from_millis(HELPER_CONNECT_TIMEOUT_MS)))?;
        stream.set_write_timeout(Some(Duration::from_millis(HELPER_CONNECT_TIMEOUT_MS)))?;
        write_json_line(&mut stream, request)?;
        stream.shutdown(std::net::Shutdown::Write)?;
        read_json_line(stream)
    }
    #[cfg(not(unix))]
    {
        let _ = (socket_path, request);
        Err(HeddleError::Config(
            "mount daemon unix socket is unsupported on this host".into(),
        ))
    }
}

fn write_json_line<W, Req>(writer: &mut W, request: &Req) -> Result<(), HeddleError>
where
    W: Write,
    Req: Serialize,
{
    serde_json::to_writer(&mut *writer, request)
        .map_err(|error| HeddleError::Config(format!("encode helper request: {error}")))?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn read_json_line<R, Resp>(reader: R) -> Result<Resp, HeddleError>
where
    R: std::io::Read,
    Resp: DeserializeOwned,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line)
        .map_err(|error| HeddleError::Config(format!("decode helper response: {error}")))
}

#[cfg(test)]
mod tests {
    use super::send_mount_daemon_request;
    use crate::daemon::{EndpointState, MountDaemonRequest};

    #[test]
    fn mount_client_refuses_tcp_endpoint_without_socket() {
        let endpoint = EndpointState {
            version: 3,
            host: "127.0.0.1".to_string(),
            port: 9,
            pid: Some(1),
            socket_path: None,
        };
        let error = send_mount_daemon_request::<_, serde_json::Value>(
            &endpoint,
            &MountDaemonRequest::Health {},
        )
        .expect_err("TCP-only endpoint must fail closed");
        assert!(
            error.to_string().contains("refusing localhost TCP"),
            "got {error}"
        );
    }
}
