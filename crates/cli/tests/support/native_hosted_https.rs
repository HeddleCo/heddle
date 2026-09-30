// SPDX-License-Identifier: Apache-2.0
//! HTTPS descriptor discovery for the native hosted CLI fixture.
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use api::{
    HOSTED_ALPN_V1,
    descriptor_trust::ephemeral_attestation_bytes,
    heddle::api::common::{EndpointDescriptor, SignedEndpointDescriptor},
    signing::endpoint_descriptor_bytes,
};
use crypto::{Ed25519Signer, Signer};
use prost::Message;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
pub fn signed_descriptor(
    endpoint_id: &str,
    direct_address: &str,
    root: &Ed25519Signer,
    ephemeral: &Ed25519Signer,
) -> Vec<u8> {
    let now = chrono::Utc::now().timestamp_millis();
    let public_key: [u8; 32] = hex::decode(endpoint_id)
        .expect("fixture endpoint id is hex")
        .try_into()
        .expect("fixture endpoint id is 32 bytes");
    let not_before = now - 1_000;
    let not_after = now + 60_000;
    let descriptor = EndpointDescriptor {
        version: 1,
        endpoint_id: hex::encode(public_key),
        relay_urls: Vec::new(),
        direct_addresses: vec![direct_address.to_string()],
        supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
        issued_at_unix_millis: not_before,
        expires_at_unix_millis: not_after,
        rotation: None,
    };
    let descriptor_signature = ephemeral
        .sign(&endpoint_descriptor_bytes(&descriptor))
        .expect("sign fixture endpoint descriptor");
    let signed = SignedEndpointDescriptor {
        descriptor: Some(descriptor),
        key_id: "clone-ephemeral".to_string(),
        signature: descriptor_signature,
    };
    let attestation_signature = root
        .sign(&ephemeral_attestation_bytes(
            "clone-ephemeral",
            &public_key,
            not_before,
            not_after,
            "test",
        ))
        .expect("sign fixture root attestation");
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "root_key_id": "clone-test-key",
        "entries": [{
            "ephemeral_key_id": "clone-ephemeral",
            "ephemeral_public_key": hex::encode(public_key),
            "not_before_unix_millis": not_before,
            "not_after_unix_millis": not_after,
            "region": "test",
            "attestation_signature": hex::encode(attestation_signature),
            "signed_descriptor": hex::encode(signed.encode_to_vec()),
        }],
    }))
    .expect("encode fixture ephemeral descriptor set")
}

pub struct TestHttpsServer {
    pub authority: String,
    pub certificate_pem: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestHttpsServer {
    pub fn start(routes: HashMap<String, VecDeque<Vec<u8>>>) -> Self {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".to_string()])
                .expect("generate test TLS certificate");
        let certificate_pem = cert.pem();
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let tls = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], private_key)
                .expect("configure test HTTPS server"),
        );
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("bind endpoint descriptor HTTPS server");
        listener
            .set_nonblocking(true)
            .expect("make endpoint descriptor listener nonblocking");
        let authority = listener.local_addr().expect("HTTPS address").to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let routes = Arc::new(Mutex::new(routes));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = Arc::clone(&requests);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve_https(
                        stream,
                        Arc::clone(&tls),
                        Arc::clone(&routes),
                        Arc::clone(&thread_requests),
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("endpoint descriptor HTTPS accept failed: {error}"),
                }
            }
        });
        Self {
            authority,
            certificate_pem,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for TestHttpsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("endpoint descriptor HTTPS server exits");
        }
    }
}

fn serve_https(
    stream: TcpStream,
    tls: Arc<ServerConfig>,
    routes: Arc<Mutex<HashMap<String, VecDeque<Vec<u8>>>>>,
    requests: Arc<Mutex<Vec<String>>>,
) {
    // Accepted sockets inherit O_NONBLOCK from the listener on macOS, but
    // this synchronous rustls fixture expects blocking handshakes.
    stream
        .set_nonblocking(false)
        .expect("make endpoint descriptor connection blocking");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set endpoint descriptor read timeout");
    let connection = ServerConnection::new(tls).expect("create test TLS connection");
    let mut stream = StreamOwned::new(connection, stream);
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = match stream.read(&mut chunk) {
            Ok(count) => count,
            Err(_) => return,
        };
        if count == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..count]);
    }
    let request = String::from_utf8_lossy(&request);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    requests
        .lock()
        .expect("record HTTP request")
        .push(path.to_string());
    let body = routes
        .lock()
        .expect("lock endpoint descriptor routes")
        .get_mut(path)
        .and_then(VecDeque::pop_front)
        .unwrap_or_default();
    let status = if body.is_empty() {
        "404 Not Found"
    } else {
        "200 OK"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let response_written = stream
        .write_all(response.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .and_then(|_| stream.flush());
    if response_written.is_ok() {
        stream.conn.send_close_notify();
        let _ = stream
            .sock
            .set_read_timeout(Some(Duration::from_millis(250)));
        let _ = stream.conn.complete_io(&mut stream.sock);
    }
}
