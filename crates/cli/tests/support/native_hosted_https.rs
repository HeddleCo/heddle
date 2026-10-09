// SPDX-License-Identifier: Apache-2.0
//! HTTPS descriptor discovery for the native hosted CLI fixture.
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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

/// The wire requires a DNS HTTPS origin without a port. A loopback CONNECT
/// proxy lets these fixtures use that exact origin without privileged binds.
struct FixtureProxy {
    uri: String,
    routes: Arc<Mutex<HashMap<String, SocketAddr>>>,
}
fn fixture_proxy() -> &'static FixtureProxy {
    static PROXY: OnceLock<FixtureProxy> = OnceLock::new();
    PROXY.get_or_init(|| {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fixture proxy");
        let uri = format!("http://{}", listener.local_addr().expect("proxy address"));
        let routes = Arc::new(Mutex::new(HashMap::<String, SocketAddr>::new()));
        let destinations = Arc::clone(&routes);
        thread::spawn(move || {
            for incoming in listener.incoming() {
                let mut client = incoming.expect("proxy client");
                let destinations = Arc::clone(&destinations);
                thread::spawn(move || {
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if client.read_exact(&mut byte).is_err() {
                            return;
                        }
                        header.push(byte[0]);
                        assert!(header.len() < 4096, "bounded CONNECT request");
                    }
                    let text = String::from_utf8(header).expect("CONNECT UTF-8");
                    let authority = text.split_whitespace().nth(1).expect("CONNECT authority");
                    let destination = destinations
                        .lock()
                        .expect("proxy routes")
                        .get(authority)
                        .copied()
                        .expect("registered fixture origin");
                    let mut upstream = TcpStream::connect(destination).expect("fixture tunnel");
                    client
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .expect("CONNECT accepted");
                    let mut client_read = client.try_clone().expect("client read half");
                    let mut upstream_write = upstream.try_clone().expect("upstream write half");
                    thread::spawn(move || {
                        let _ = std::io::copy(&mut client_read, &mut upstream_write);
                        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
                    });
                    let _ = std::io::copy(&mut upstream, &mut client);
                    let _ = client.shutdown(std::net::Shutdown::Write);
                });
            }
        });
        // All fixtures in this test process share this immutable proxy. CLI
        // children receive it explicitly; no production routing knob is added.
        unsafe {
            std::env::set_var("HTTPS_PROXY", &uri);
        }
        FixtureProxy { uri, routes }
    })
}

pub struct TestHttpsServer {
    pub authority: String,
    pub certificate_pem: String,
    pub proxy_uri: String,
    pub connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestHttpsServer {
    pub fn start_with<R>(routes_for: impl FnOnce(&str) -> R) -> Self
    where
        R: FnMut(&str) -> Option<Vec<u8>> + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fixture HTTPS listener");
        let address = listener.local_addr().expect("fixture address");
        let authority = format!("native-{}.test", uuid::Uuid::now_v7().simple());
        let proxy = fixture_proxy();
        proxy
            .routes
            .lock()
            .expect("proxy routes")
            .insert(format!("{authority}:443"), address);
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec![authority.clone()])
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
        listener
            .set_nonblocking(true)
            .expect("make endpoint descriptor listener nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let routes = Arc::new(Mutex::new(routes_for(&authority)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = Arc::clone(&requests);
        let connections = Arc::new(AtomicUsize::new(0));
        let thread_connections = Arc::clone(&connections);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread_connections.fetch_add(1, Ordering::SeqCst);
                        let tls = Arc::clone(&tls);
                        let routes = Arc::clone(&routes);
                        let requests = Arc::clone(&thread_requests);
                        thread::spawn(move || serve_https(stream, tls, routes, requests));
                    }
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
            proxy_uri: proxy.uri.clone(),
            connections,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for TestHttpsServer {
    fn drop(&mut self) {
        fixture_proxy()
            .routes
            .lock()
            .expect("proxy routes")
            .remove(&format!("{}:443", self.authority));
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("endpoint descriptor HTTPS server exits");
        }
    }
}

fn serve_https<R>(
    stream: TcpStream,
    tls: Arc<ServerConfig>,
    routes: Arc<Mutex<R>>,
    requests: Arc<Mutex<Vec<String>>>,
) where
    R: FnMut(&str) -> Option<Vec<u8>>,
{
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
    loop {
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
    let body = routes.lock().expect("lock endpoint descriptor routes")(path).unwrap_or_default();
    let status = if body.is_empty() {
        "404 Not Found"
    } else {
        "200 OK"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response_written = stream
        .write_all(response.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .and_then(|_| stream.flush());
    if response_written.is_err() {
        return;
    }
    }
}
