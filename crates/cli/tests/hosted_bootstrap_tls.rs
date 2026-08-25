// SPDX-License-Identifier: Apache-2.0
//! Reproduce and lock hosted bootstrap TLS CA honoring for `heddle auth login`.
//!
//! A private-CA HTTPS server is enough: login fails at descriptor fetch before
//! any Iroh session. UnknownIssuer without a trusted CA, then a later
//! descriptor-trust error once the CA is honored, is the evidence that the
//! knob reached the bootstrap request.

#![cfg(feature = "client")]

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
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
use tempfile::TempDir;

struct PrivateCaHostedServer {
    authority: String,
    ca_pem: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PrivateCaHostedServer {
    fn spawn() -> Self {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".to_string()])
                .expect("generate private-CA leaf");
        let ca_pem = cert.pem();
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let tls = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], private_key)
                .expect("private-CA HTTPS server config"),
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind private-CA HTTPS");
        listener
            .set_nonblocking(true)
            .expect("nonblocking private-CA HTTPS listener");
        let authority = format!("https://{}", listener.local_addr().expect("HTTPS address"));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve_404(stream, Arc::clone(&tls)),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("private-CA HTTPS accept failed: {error}"),
                }
            }
        });
        Self {
            authority,
            ca_pem,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for PrivateCaHostedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_404(stream: TcpStream, tls: Arc<ServerConfig>) {
    stream
        .set_nonblocking(false)
        .expect("blocking private-CA HTTPS connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("private-CA HTTPS read timeout");
    let connection = ServerConnection::new(tls).expect("private-CA TLS connection");
    let mut stream = StreamOwned::new(connection, stream);
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => request.extend_from_slice(&chunk[..count]),
        }
    }
    let response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    if stream
        .write_all(response)
        .and_then(|_| stream.flush())
        .is_ok()
    {
        stream.conn.send_close_notify();
        let _ = stream
            .sock
            .set_read_timeout(Some(Duration::from_millis(250)));
        let _ = stream.conn.complete_io(&mut stream.sock);
    }
}

struct LoginFixture {
    ca_path: PathBuf,
    repo: PathBuf,
    server: String,
    temp: TempDir,
    _https: PrivateCaHostedServer,
}

impl LoginFixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        let init = heddle_command(&temp, &repo)
            .args(["init"])
            .output()
            .expect("init repo");
        assert!(init.status.success(), "init: {}", stderr(&init));

        let https = PrivateCaHostedServer::spawn();
        let ca_path = temp.path().join("private-ca.pem");
        std::fs::write(&ca_path, &https.ca_pem).expect("write private CA");
        Self {
            server: https.authority.clone(),
            ca_path,
            repo,
            temp,
            _https: https,
        }
    }

    fn login(&self, ca: CaSource) -> Output {
        let mut command = heddle_command(&self.temp, &self.repo);
        command.args(["auth", "login", "--open-browser", "--server", &self.server]);
        match ca {
            CaSource::None => {}
            CaSource::Env => {
                command.env("HEDDLE_REMOTE_TLS_CA_CERT", &self.ca_path);
            }
            CaSource::UserConfig => {
                let config = self.temp.path().join("user-ca.toml");
                std::fs::write(
                    &config,
                    format!(
                        "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n\n[remote]\ntls_ca_certificate_path = \"{}\"\n",
                        self.ca_path.display()
                    ),
                )
                .expect("write user config CA");
                command.env("HEDDLE_CONFIG", config);
            }
            CaSource::RepoConfig => write_repo_tls_ca(&self.repo, &self.ca_path),
        }
        command.output().expect("run heddle auth login")
    }
}

#[derive(Clone, Copy)]
enum CaSource {
    None,
    Env,
    UserConfig,
    RepoConfig,
}

#[test]
fn auth_login_without_ca_names_remote_tls_configuration() {
    let fixture = LoginFixture::new();
    let output = fixture.login(CaSource::None);
    let err = stderr(&output);
    assert!(!output.status.success(), "untrusted private CA must fail");
    assert!(
        is_tls_trust_failure(&err),
        "untrusted login must be a TLS trust failure: {err}"
    );
    assert!(
        err.contains("HEDDLE_REMOTE_TLS_CA_CERT"),
        "UnknownIssuer must name the CA configuration: {err}"
    );
    assert!(
        !err.contains("heddle status"),
        "hosted TLS trust failure must not hint heddle status: {err}"
    );
    eprintln!("untrusted login names the CA configuration:\n{err}");
}

#[test]
fn auth_login_honours_user_config_tls_ca_certificate_path() {
    let fixture = LoginFixture::new();
    let output = fixture.login(CaSource::UserConfig);
    assert_tls_honored(&output, "user config");
}

#[test]
fn auth_login_honours_repo_config_tls_ca_certificate_path() {
    let fixture = LoginFixture::new();
    let output = fixture.login(CaSource::RepoConfig);
    assert_tls_honored(&output, "repo config");
}

#[test]
fn auth_login_honours_dash_c_repo_ca_not_cwd() {
    let fixture = LoginFixture::new();
    write_repo_tls_ca(&fixture.repo, &fixture.ca_path);

    let cwd_repo = fixture.temp.path().join("cwd-repo");
    std::fs::create_dir_all(&cwd_repo).expect("create cwd repo dir");
    let init = heddle_command(&fixture.temp, &cwd_repo)
        .args(["init"])
        .output()
        .expect("init cwd repo");
    assert!(init.status.success(), "init cwd repo: {}", stderr(&init));

    let CertifiedKey { cert, .. } =
        generate_simple_self_signed(vec!["127.0.0.1".to_string()]).expect("cwd decoy CA");
    let cwd_ca = fixture.temp.path().join("cwd-ca.pem");
    std::fs::write(&cwd_ca, cert.pem()).expect("write cwd decoy CA");
    write_repo_tls_ca(&cwd_repo, &cwd_ca);

    let output = heddle_command(&fixture.temp, &cwd_repo)
        .arg("-C")
        .arg(&fixture.repo)
        .args([
            "auth",
            "login",
            "--open-browser",
            "--server",
            &fixture.server,
        ])
        .output()
        .expect("run heddle -C auth login");
    assert_tls_honored(&output, "-C selected repo config");
}

#[test]
fn auth_login_honours_env_tls_ca_cert() {
    let fixture = LoginFixture::new();
    let output = fixture.login(CaSource::Env);
    assert_tls_honored(&output, "HEDDLE_REMOTE_TLS_CA_CERT");
}

fn write_repo_tls_ca(repo: &Path, ca_path: &Path) {
    let repo_config = repo.join(".heddle/config.toml");
    let current = std::fs::read_to_string(&repo_config).expect("read repo config");
    std::fs::write(
        repo_config,
        format!(
            "{current}\n[remote]\ntls_ca_certificate_path = \"{}\"\n",
            ca_path.display()
        ),
    )
    .expect("write repo config CA");
}

fn assert_tls_honored(output: &Output, source: &str) {
    let err = stderr(output);
    assert!(
        !output.status.success(),
        "{source}: fixture has no descriptor document, so login must still fail: {err}"
    );
    assert!(
        !is_tls_trust_failure(&err),
        "{source}: configured CA must be honored so login fails after TLS: {err}"
    );
    assert!(
        err.contains("descriptor trust") || err.contains("descriptor"),
        "{source}: post-TLS failure should name descriptor trust: {err}"
    );
    eprintln!("{source}: TLS honored; login failed after bootstrap:\n{err}");
}

fn is_tls_trust_failure(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("invalid peer certificate")
        || normalized.contains("unknownissuer")
        || normalized.contains("unknown issuer")
        || normalized.contains("certificateunknown")
}

fn heddle_command(temp: &TempDir, cwd: &Path) -> Command {
    let config = temp.path().join("config.toml");
    if !config.exists() {
        std::fs::write(
            &config,
            "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n",
        )
        .expect("write Heddle config");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .current_dir(cwd)
        .env("PATH", "")
        .env("HOME", temp.path())
        .env("HEDDLE_HOME", temp.path().join("heddle-home"))
        .env("HEDDLE_CONFIG", &config)
        .env("NO_COLOR", "1")
        .env_remove("HEDDLE_REMOTE_TLS_CA_CERT")
        .env_remove("SSL_CERT_FILE")
        .env_remove("SSL_CERT_DIR");
    command
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
