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

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};

#[derive(Clone)]
pub(super) struct TestResponse {
    status: u16,
    content_type: Option<&'static str>,
    location: Option<String>,
    body: Vec<u8>,
}

impl TestResponse {
    pub(super) fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: Some("application/json"),
            location: None,
            body: body.into(),
        }
    }

    pub(super) fn protobuf(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: Some("application/protobuf"),
            location: None,
            body: body.into(),
        }
    }

    pub(super) fn status(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            location: None,
            body: Vec::new(),
        }
    }

    pub(super) fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: 302,
            content_type: None,
            location: Some(location.into()),
            body: Vec::new(),
        }
    }
}

pub(super) struct TestHttpsServer {
    authority: String,
    certificate_pem: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestHttpsServer {
    pub(super) fn start(routes: HashMap<String, VecDeque<TestResponse>>) -> Self {
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
                .expect("test TLS server config"),
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test HTTPS server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking test HTTPS listener");
        let authority = format!("https://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let routes = Arc::new(Mutex::new(routes));
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        serve_connection(
                            stream,
                            Arc::clone(&tls),
                            Arc::clone(&routes),
                            Arc::clone(&thread_requests),
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test HTTPS accept failed: {error}"),
                }
            }
        });
        Self {
            authority,
            certificate_pem,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub(super) fn authority(&self) -> &str {
        &self.authority
    }

    pub(super) fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    pub(super) fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TestHttpsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_connection(
    stream: TcpStream,
    tls: Arc<ServerConfig>,
    routes: Arc<Mutex<HashMap<String, VecDeque<TestResponse>>>>,
    requests: Arc<Mutex<Vec<String>>>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("test HTTPS read timeout");
    let connection = ServerConnection::new(tls).expect("test TLS connection");
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
    let request_line = String::from_utf8_lossy(&request);
    let path = request_line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    requests.lock().unwrap().push(path.clone());
    let response = routes
        .lock()
        .unwrap()
        .get_mut(&path)
        .and_then(VecDeque::pop_front)
        .unwrap_or_else(|| TestResponse::status(404));
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Test Status",
    };
    let mut headers = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    if let Some(content_type) = response.content_type {
        headers.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if let Some(location) = response.location {
        headers.push_str(&format!("Location: {location}\r\n"));
    }
    headers.push_str("\r\n");
    let _ = stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.write_all(&response.body))
        .and_then(|_| stream.flush());
}
