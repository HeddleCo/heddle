// SPDX-License-Identifier: Apache-2.0

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};

pub struct PrivateCaGitServer {
    ca_pem: String,
    join: Option<thread::JoinHandle<()>>,
    port: u16,
    stop: Arc<AtomicBool>,
}

impl PrivateCaGitServer {
    pub fn spawn(root: &Path) -> Self {
        let mut ca_params = CertificateParams::new(Vec::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().expect("CA key");
        let ca_cert = ca_params.self_signed(&ca_key).expect("CA certificate");
        let issuer = Issuer::new(ca_params, ca_key);

        let mut leaf_params =
            CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("leaf params");
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_key = KeyPair::generate().expect("leaf key");
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &issuer)
            .expect("CA-signed leaf");
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let tls = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![leaf_cert.der().clone(), ca_cert.der().clone()],
                    private_key,
                )
                .expect("TLS server config"),
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTPS Git server");
        let port = listener.local_addr().expect("listener address").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let root = root.to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve_git_request(stream, Arc::clone(&tls), &root),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("HTTPS Git accept failed: {error}"),
                }
            }
        });
        Self {
            ca_pem: ca_cert.pem(),
            join: Some(join),
            port,
            stop,
        }
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    pub fn url(&self) -> String {
        format!("https://127.0.0.1:{}/source.git", self.port)
    }
}

impl Drop for PrivateCaGitServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.join().expect("HTTPS Git server thread");
        }
    }
}

fn serve_git_request(stream: TcpStream, tls: Arc<ServerConfig>, root: &Path) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("HTTPS read timeout");
    let connection = ServerConnection::new(tls).expect("TLS connection");
    let mut stream = StreamOwned::new(connection, stream);
    let Some((method, path, query, content_type, body)) = read_http_request(&mut stream) else {
        return;
    };
    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", content_type)
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git http-backend");
    child
        .stdin
        .take()
        .expect("backend stdin")
        .write_all(&body)
        .expect("write backend request");
    let output = child.wait_with_output().expect("wait for git http-backend");
    assert!(output.status.success(), "git http-backend failed");
    write_cgi_response(&mut stream, &output.stdout);
}

fn read_http_request(stream: &mut impl Read) -> Option<(String, String, String, String, Vec<u8>)> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(offset) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut content_length = 0;
    let mut content_type = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().expect("content length");
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = value.trim().to_string();
        }
    }
    let mut body = request[header_end..].to_vec();
    while body.len() < content_length {
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..count]);
    }
    body.truncate(content_length);
    Some((
        method,
        path.to_string(),
        query.to_string(),
        content_type,
        body,
    ))
}

fn write_cgi_response(stream: &mut impl Write, response: &[u8]) {
    let split = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|offset| offset + 4)
        .or_else(|| {
            response
                .windows(2)
                .position(|part| part == b"\n\n")
                .map(|offset| offset + 2)
        })
        .expect("CGI headers");
    let headers_text = String::from_utf8_lossy(&response[..split]);
    let mut status = "200 OK".to_string();
    let mut headers = Vec::new();
    for line in headers_text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("Status:") {
            status = value.trim().to_string();
        } else if !line.is_empty() {
            headers.push(line);
        }
    }
    write!(stream, "HTTP/1.1 {status}\r\n").expect("write status");
    for header in headers {
        write!(stream, "{header}\r\n").expect("write header");
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.len() - split
    )
    .expect("write response headers");
    stream
        .write_all(&response[split..])
        .expect("write response body");
}
