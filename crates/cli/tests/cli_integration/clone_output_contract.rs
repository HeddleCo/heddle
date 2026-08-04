// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "client")]

mod fixture {
    use std::{
        collections::{HashMap, VecDeque},
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use api::{
        HOSTED_ALPN_V1,
        framing::{decode_request_prelude, encode_failure_response},
        heddle::api::v1alpha1::{
            CallFailure, CallFailureCode, EndpointDescriptor, SignedEndpointDescriptor,
        },
        signing::endpoint_descriptor_bytes,
    };
    use biscuit_auth::KeyPair;
    use crypto::{Ed25519Signer, Signer};
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use prost::Message;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{
        ServerConfig, ServerConnection, StreamOwned,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    };
    use serde_json::Value;
    use tempfile::TempDir;

    use super::super::heddle_output_with_env;

    const DESCRIPTOR_PATH: &str = "/.well-known/heddle/iroh-endpoint";

    #[test]
    fn clone_json_peer_disconnect_after_connection_emits_structured_error() {
        let temp = TempDir::new().expect("create clone contract fixture root");
        let (endpoint_id, direct_address, iroh_thread) = disconnecting_iroh_server(None);
        let signer = Ed25519Signer::generate().expect("generate descriptor signer");
        let descriptor = signed_descriptor(&endpoint_id, &direct_address, &signer);
        let https = TestHttpsServer::start(HashMap::from([(
            DESCRIPTOR_PATH.to_string(),
            VecDeque::from([descriptor]),
        )]));
        let certificate = temp.path().join("descriptor-ca.pem");
        std::fs::write(&certificate, &https.certificate_pem).expect("write descriptor test CA");
        let destination = temp.path().join("clone");
        let remote = format!("heddle://{}/owner/repo", https.authority);
        let descriptor_public_key = hex::encode(signer.public_key());
        let heddle_home = temp.path().join("heddle-home");

        let output = heddle_output_with_env(
            &[
                "--output",
                "json",
                "clone",
                &remote,
                destination.to_str().expect("UTF-8 destination"),
            ],
            Some(temp.path()),
            &[
                (
                    "HEDDLE_REMOTE_TLS_CA_CERT",
                    certificate.to_str().expect("UTF-8 CA path"),
                ),
                ("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID", "clone-test-key"),
                (
                    "HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
                    &descriptor_public_key,
                ),
                ("HEDDLE_HOME", heddle_home.to_str().expect("UTF-8 home")),
            ],
        )
        .expect("invoke clone against disconnecting fixture");

        iroh_thread.join().expect("disconnecting Iroh server exits");
        assert!(
            !output.status.success(),
            "mid-clone peer disconnect must exit non-zero\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(75));
        let records = String::from_utf8(output.stdout).expect("clone stdout is UTF-8");
        let records = records
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("clone stdout record is JSON"))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["output_kind"], "clone_connection");
        assert_eq!(records[0]["status"], "connected");

        let error: Value = serde_json::from_slice(&output.stderr)
            .expect("mid-clone disconnect must emit a structured JSON error");
        assert_eq!(error["exit_code"], output.status.code().unwrap());
        assert!(
            error["error"]
                .as_str()
                .is_some_and(|message| message.contains("Broken pipe")),
            "error envelope must retain the peer disconnect: {error}"
        );
        assert!(
            repo::clone_intent::CloneIntent::path(&destination).is_file(),
            "a connected interrupted clone retains its durable recovery intent"
        );
        assert!(matches!(
            repo::Repository::open(&destination),
            Err(repo::HeddleError::IncompleteClone(path)) if path == destination
        ));
    }

    #[test]
    fn authenticated_clone_starts_with_folded_pull_instead_of_list_refs() {
        const PULL: &str = "/heddle.api.v1alpha1.RepoSyncService/Pull";

        let temp = TempDir::new().expect("create folded clone fixture root");
        let (endpoint_id, direct_address, iroh_thread) = disconnecting_iroh_server(Some(PULL));
        let signer = Ed25519Signer::generate().expect("generate descriptor signer");
        let descriptor = signed_descriptor(&endpoint_id, &direct_address, &signer);
        let https = TestHttpsServer::start(HashMap::from([(
            DESCRIPTOR_PATH.to_string(),
            VecDeque::from([descriptor]),
        )]));
        let certificate = temp.path().join("descriptor-ca.pem");
        std::fs::write(&certificate, &https.certificate_pem).expect("write descriptor test CA");
        let credential = temp.path().join("clone-test.hcred");
        write_test_credential(&credential, &https.authority);
        let destination = temp.path().join("clone");
        let remote = format!("heddle://{}/owner/repo", https.authority);
        let descriptor_public_key = hex::encode(signer.public_key());
        let heddle_home = temp.path().join("heddle-home");

        let output = heddle_output_with_env(
            &[
                "clone",
                &remote,
                destination.to_str().expect("UTF-8 destination"),
            ],
            Some(temp.path()),
            &[
                (
                    "HEDDLE_REMOTE_TLS_CA_CERT",
                    certificate.to_str().expect("UTF-8 CA path"),
                ),
                ("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID", "clone-test-key"),
                (
                    "HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
                    &descriptor_public_key,
                ),
                ("HEDDLE_HOME", heddle_home.to_str().expect("UTF-8 home")),
                (
                    "HEDDLE_CREDENTIAL",
                    credential.to_str().expect("UTF-8 credential path"),
                ),
            ],
        )
        .expect("invoke authenticated clone against route fixture");

        iroh_thread.join().expect("route-check Iroh server exits");
        assert!(
            !output.status.success(),
            "fixture terminates the folded Pull"
        );
        assert!(
            repo::clone_intent::CloneIntent::path(&destination).is_file(),
            "folded-pull interruption must remain detectable and resumable"
        );
    }

    fn write_test_credential(path: &std::path::Path, server: &str) {
        let signer = Ed25519Signer::generate().expect("generate credential proof key");
        let token = biscuit_auth::Biscuit::builder()
            .fact(r#"user("clone-test")"#)
            .expect("credential subject fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
            .expect("credential proof-key fact")
            .build(&KeyPair::new())
            .expect("mint test credential")
            .to_base64()
            .expect("encode test credential");
        let encoded = serde_json::to_vec_pretty(&serde_json::json!({
            "format": "heddle-credential",
            "version": 1,
            "server": server,
            "kind": "device",
            "subject": "clone-test",
            "token": token,
            "proof_key_pem": signer.to_pem().expect("encode credential proof key"),
            "credential_id": null,
        }))
        .expect("encode credential file");
        std::fs::write(path, encoded).expect("write credential file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict credential file permissions");
        }
    }

    fn disconnecting_iroh_server(
        expected_method: Option<&'static str>,
    ) -> (String, String, thread::JoinHandle<()>) {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build Iroh fixture runtime");
            runtime.block_on(async move {
                let endpoint = Endpoint::builder(presets::Minimal)
                    .alpns(vec![HOSTED_ALPN_V1.to_vec()])
                    .relay_mode(RelayMode::Disabled)
                    .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
                    .expect("bind Iroh fixture address")
                    .bind()
                    .await
                    .expect("start Iroh fixture endpoint");
                let address = endpoint.addr();
                let direct = address
                    .ip_addrs()
                    .next()
                    .expect("fixture endpoint has a direct address")
                    .to_string();
                ready_tx
                    .send((endpoint.id().to_string(), direct))
                    .expect("publish Iroh fixture address");

                let incoming = endpoint.accept().await.expect("accept clone connection");
                let connection = incoming.await.expect("complete clone connection");
                let (mut send, mut recv) = connection
                    .accept_bi()
                    .await
                    .expect("accept initial Pull RPC");
                let mut request = vec![0_u8; 70 * 1024];
                let mut received = 0;
                let method = loop {
                    let read = recv
                        .read(&mut request[received..])
                        .await
                        .expect("read clone request prelude")
                        .expect("clone request ended before its prelude");
                    received += read;
                    if let Some((prelude, _)) = decode_request_prelude(&request[..received])
                        .expect("decode request prelude")
                    {
                        break prelude.method.to_string();
                    }
                    assert!(
                        received < request.len(),
                        "clone request prelude is oversized"
                    );
                };
                if let Some(expected_method) = expected_method {
                    assert_eq!(
                        method, expected_method,
                        "an authenticated clone must start with folded Pull, not ListRefs"
                    );
                }
                let failure = encode_failure_response(&CallFailure {
                    code: CallFailureCode::Unavailable as i32,
                    message: "Broken pipe".to_string(),
                    error: None,
                })
                .expect("encode disconnect failure");
                send.write_all(&failure)
                    .await
                    .expect("send disconnect failure");
                send.finish().expect("finish disconnect response");
                tokio::time::sleep(Duration::from_millis(20)).await;
                connection.close(1_u32.into(), b"Broken pipe");
                connection.closed().await;
                endpoint.close().await;
            });
        });
        let (endpoint_id, direct_address) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("Iroh fixture starts");
        (endpoint_id, direct_address, thread)
    }

    fn signed_descriptor(
        endpoint_id: &str,
        direct_address: &str,
        signer: &Ed25519Signer,
    ) -> Vec<u8> {
        let now = chrono::Utc::now().timestamp_millis();
        let descriptor = EndpointDescriptor {
            version: 1,
            endpoint_id: endpoint_id.to_string(),
            relay_urls: Vec::new(),
            direct_addresses: vec![direct_address.to_string()],
            supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
            issued_at_unix_millis: now - 1_000,
            expires_at_unix_millis: now + 60_000,
            rotation: None,
        };
        SignedEndpointDescriptor {
            signature: signer
                .sign(&endpoint_descriptor_bytes(&descriptor))
                .expect("sign fixture endpoint descriptor"),
            descriptor: Some(descriptor),
            key_id: "clone-test-key".to_string(),
        }
        .encode_to_vec()
    }

    struct TestHttpsServer {
        authority: String,
        certificate_pem: String,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestHttpsServer {
        fn start(routes: HashMap<String, VecDeque<Vec<u8>>>) -> Self {
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
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            serve_https(stream, Arc::clone(&tls), Arc::clone(&routes))
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
    ) {
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
            "HTTP/1.1 {status}\r\nContent-Type: application/protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream
            .write_all(response.as_bytes())
            .and_then(|_| stream.write_all(&body))
            .and_then(|_| stream.flush());
    }
}
