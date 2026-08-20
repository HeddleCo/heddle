// SPDX-License-Identifier: Apache-2.0
//! Unix-domain listener loop for the mount daemon.
//!
//! Parallel to the TCP helper loop, plus a same-uid peer check on
//! every accept. The mount daemon binds only this path.

use std::{
    io::{ErrorKind, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    time::{Duration, Instant},
};

use objects::error::HeddleError;

use super::{
    mount_proto::{ERR_UNAUTHORIZED, MOUNT_PROTOCOL_VERSION, MountDaemonResponse},
    peer::check_peer_uid_matches_self,
    protocol::HELPER_IDLE_POLL_MS,
    server::{IdleDecision, handle_json_rw},
};

/// Per-connection hook for a Unix-domain helper daemon.
pub trait UnixDaemonHandler {
    fn handle(&mut self, stream: UnixStream) -> Result<(), HeddleError>;
    fn on_tick(&mut self, idle_for: Duration) -> IdleDecision;
}

/// Bind `path` as a mode-0600 Unix socket. A leftover socket from a
/// crashed daemon may be replaced. A live, connectable socket is
/// left intact — fail closed so a second `daemon serve` cannot steal
/// the endpoint from a running daemon.
pub fn bind_unix_socket(path: &Path) -> Result<UnixListener, HeddleError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match bind_mode_0600(path) {
        Ok(listener) => Ok(listener),
        Err(error) if is_addr_in_use(&error) => {
            if unix_socket_is_live(path) {
                return Err(HeddleError::Config(format!(
                    "refusing to replace live mount daemon socket {}",
                    path.display()
                )));
            }
            remove_stale_unix_socket(path)?;
            bind_mode_0600(path).map_err(Into::into)
        }
        Err(error) => Err(error.into()),
    }
}

fn bind_mode_0600(path: &Path) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn is_addr_in_use(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::AddrInUse | ErrorKind::AlreadyExists
    )
}

/// True when something is accepting on `path`. Used to distinguish a
/// crashed leftover from a live daemon before unlinking.
pub fn unix_socket_is_live(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

fn remove_stale_unix_socket(path: &Path) -> Result<(), HeddleError> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(HeddleError::Config(format!(
            "refusing to reuse {}: not a unix socket",
            path.display()
        ))),
    }
}

/// Drive `listener` until the handler returns [`IdleDecision::Exit`].
pub fn run_unix_server_loop<H: UnixDaemonHandler>(
    listener: &UnixListener,
    handler: &mut H,
) -> Result<(), HeddleError> {
    let mut last_activity = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                last_activity = Instant::now();
                // A dropped connect (liveness probe) or a bad request
                // must not take the daemon down. Fail that connection
                // only.
                if let Err(error) = handler.handle(stream) {
                    tracing::debug!(%error, "mount daemon dropped a connection");
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                match handler.on_tick(last_activity.elapsed()) {
                    IdleDecision::Continue => {
                        std::thread::sleep(Duration::from_millis(HELPER_IDLE_POLL_MS));
                    }
                    IdleDecision::Exit => return Ok(()),
                }
            }
            Err(error) => return Err(HeddleError::Io(error)),
        }
    }
}

/// Same-uid gate, then one JSON request/response. Fail closed before
/// reading a client-supplied `mount_path` when peer identity is missing.
pub fn handle_authenticated_unix_connection<Req, Resp, Respond>(
    mut stream: UnixStream,
    respond: Respond,
) -> Result<(), HeddleError>
where
    Req: serde::de::DeserializeOwned,
    Resp: serde::Serialize,
    Respond: FnOnce(Req) -> Resp,
{
    if let Err(error) = stream.set_nonblocking(false) {
        return Err(HeddleError::Io(error));
    }
    if let Err(error) = check_peer_uid_matches_self(&stream) {
        write_unauthorized(&mut stream, &error.to_string());
        return Ok(());
    }
    let reader = stream.try_clone()?;
    handle_json_rw(reader, &mut stream, respond)
}

fn write_unauthorized(stream: &mut UnixStream, message: &str) {
    let response = MountDaemonResponse::Error {
        version: MOUNT_PROTOCOL_VERSION,
        code: ERR_UNAUTHORIZED.to_string(),
        message: message.to_string(),
    };
    if serde_json::to_writer(&mut *stream, &response).is_ok() {
        let _ = stream.write_all(b"\n");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixStream,
    };

    use tempfile::TempDir;

    use super::{bind_unix_socket, handle_authenticated_unix_connection, unix_socket_is_live};
    use crate::daemon::{
        ERR_UNAUTHORIZED, MOUNT_PROTOCOL_VERSION, MountDaemonRequest, MountDaemonResponse,
    };

    #[test]
    fn bind_unix_socket_refuses_to_unlink_a_live_socket() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("heddled.sock");
        let first = bind_unix_socket(&path).expect("first bind");
        let error = bind_unix_socket(&path).expect_err("live socket must not be stolen");
        assert!(
            error.to_string().contains("live mount daemon socket"),
            "got {error}"
        );
        assert!(
            unix_socket_is_live(&path),
            "original listener must still own the socket name"
        );
        drop(first);
    }

    #[test]
    fn bind_unix_socket_replaces_a_dead_leftover_socket() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("heddled.sock");
        let first = bind_unix_socket(&path).expect("first bind");
        drop(first);
        let replacement = bind_unix_socket(&path).expect("stale leftover socket may be replaced");
        assert!(
            unix_socket_is_live(&path),
            "replacement listener must be reachable"
        );
        drop(replacement);
    }

    #[test]
    fn bind_unix_socket_rejects_a_regular_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("not-a-socket");
        std::fs::write(&path, b"nope").unwrap();
        let error = bind_unix_socket(&path).expect_err("regular file must fail closed");
        assert!(
            error.to_string().contains("not a unix socket"),
            "got {error}"
        );
    }

    #[test]
    fn authenticated_pair_serves_health() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            handle_authenticated_unix_connection(server, |request: MountDaemonRequest| {
                assert!(matches!(request, MountDaemonRequest::Health {}));
                MountDaemonResponse::Health {
                    version: MOUNT_PROTOCOL_VERSION,
                    ok: true,
                    uptime_s: 1,
                    mount_count: 0,
                }
            })
        });
        serde_json::to_writer(&mut client, &MountDaemonRequest::Health {}).unwrap();
        client.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        let decoded: MountDaemonResponse = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            decoded,
            MountDaemonResponse::Health { ok: true, .. }
        ));
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn unauthorized_error_uses_stable_code() {
        let response = MountDaemonResponse::Error {
            version: MOUNT_PROTOCOL_VERSION,
            code: ERR_UNAUTHORIZED.to_string(),
            message: "peer uid mismatch".to_string(),
        };
        let raw = serde_json::to_string(&response).unwrap();
        assert!(raw.contains(r#""code":"unauthorized""#));
    }
}
