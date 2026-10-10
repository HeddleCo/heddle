// SPDX-License-Identifier: Apache-2.0
//! Python-free, bounded native Heddle smart-HTTP gateway. Default hosted disclosure is fail-closed.
#[path = "gateway_host/catalog_publication.rs"]
pub mod catalog_publication;
#[cfg(feature = "gateway-publication")]
#[path = "gateway_host/hosted_runtime.rs"]
pub mod hosted_runtime;
#[cfg(feature = "gateway-publication")]
#[path = "gateway_host/native_frame.rs"]
pub mod native_frame;
#[cfg(feature = "gateway-publication")]
#[path = "gateway_host/native_publication.rs"]
pub mod native_publication;
#[path = "gateway_host/policy.rs"]
mod policy;
#[path = "gateway_host/projection.rs"]
mod projection;
#[cfg(feature = "gateway-publication")]
#[path = "gateway_host/runtime_trace.rs"]
mod runtime_trace;
#[cfg(feature = "gateway-publication")]
#[path = "gateway_host/session_authority.rs"]
pub mod session_authority;
#[path = "gateway_host/transport.rs"]
mod transport;
#[path = "gateway_host/window.rs"]
mod window;
use policy::{ConfigSource, Disclosure, canonical};
use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    time::Duration,
};
#[cfg(test)]
#[path = "gateway_host/visibility_tests.rs"]
mod visibility_tests;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn disclosure(
    client: &reqwest::blocking::Client,
    origin: &str,
    c: &policy::Config,
    m: &policy::Manifest,
    pin: &str,
    local: bool,
) -> Result<Disclosure> {
    let bytes = transport::bridge_read(client, origin, &format!("/disclosure/{pin}"), 16 * 1024)?;
    let d: Disclosure = serde_json::from_slice(&bytes)?;
    if canonical(&d)? != bytes {
        return Err("noncanonical disclosure response".into());
    }
    d.validate(c, m, pin, local)?;
    Ok(d)
}
fn serve(
    request: &mut transport::Request,
    source: &ConfigSource,
    client: &reqwest::blocking::Client,
    origin: &str,
    local: bool,
    cache: &mut projection::Cache,
) -> Result<()> {
    let config = source.load()?;
    let reader = request
        .header("authorization")
        .ok_or("missing reader")?
        .to_string();
    let service = request
        .header("x-gateway-service-authorization")
        .ok_or("missing service")?
        .to_string();
    let manifest = config.authorize(&request.pin, &reader, &service)?.clone();
    let raw = transport::bridge_read(
        client,
        origin,
        &format!("/catalog/{}", request.pin),
        16 * 1024,
    )?;
    if raw != canonical(&manifest)? {
        return Err("catalog differs from approved immutable view".into());
    }
    // No static policy or matching signature can stand in for current disclosure authority.
    let decision = disclosure(client, origin, &config, &manifest, &request.pin, local)?;
    let temp = tempfile::Builder::new()
        .prefix("heddle-git-request-")
        .tempdir()?;
    let git = temp.path().join("view.git");
    cache.materialize(&config, &manifest, &decision, &git, || {
        transport::bridge_read(
            client,
            origin,
            &format!("/native/{}", manifest.source),
            projection::MAX_BUNDLE,
        )
    })?;
    let response = transport::prepare_git(request, &git)?;
    // Revocation, expiry, changed closure/generation during cold work deny BEFORE any Git bytes.
    let current = source.load()?;
    let current_manifest = current.authorize(&request.pin, &reader, &service)?;
    if canonical(&config)? != canonical(&current)? || current_manifest != &manifest {
        return Err("policy changed while serving".into());
    }
    let final_decision = disclosure(
        client,
        origin,
        &current,
        current_manifest,
        &request.pin,
        local,
    )?;
    if !decision.same_generation(&final_decision) {
        return Err("disclosure changed while serving".into());
    }
    response.send(request)?;
    Ok(())
}
fn configure_tls() {
    // Every entrypoint, including hosted mode, may construct a HTTPS client.
    // Preserve a provider explicitly installed by an embedding application.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
fn run() -> Result<()> {
    configure_tls();
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--project" {
        return projection::project(std::path::Path::new(&args[2]));
    }
    if args.len() == 5 && args[1] == "--local-window" && args[3] == "--bind" {
        return window::run(std::path::Path::new(&args[2]), &args[4]);
    }
    #[cfg(feature = "gateway-publication")]
    if args.len() == 5 && args[1] == "--hosted" && args[3] == "--bind" {
        return hosted_runtime::run(std::path::Path::new(&args[2]), &args[4]);
    }
    let local = args.iter().any(|arg| arg == "--local-test");
    let mut bind = "0.0.0.0:8080".to_string();
    let mut origin = "http://gateway-bindings.internal".to_string();
    let mut config_file = None;
    let mut cache_enabled = true;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str(){
        "--local-test"=>{},"--no-cache"=>cache_enabled=false,
        "--config"|"--bind"|"--bridge"=>{let key=&args[i];i+=1;let value=args.get(i).ok_or("missing argument")?;match key.as_str(){"--config"=>config_file=Some(PathBuf::from(value)),"--bind"=>bind=value.clone(),_=>origin=value.clone()}},
        _=>return Err("usage: gateway_host [--no-cache] | --local-test --config FILE --bind 127.0.0.1:PORT --bridge http://127.0.0.1:PORT | --local-window FILE --bind 127.0.0.1:PORT".into())};
        i += 1;
    }
    let address: SocketAddr = bind.parse()?;
    if local {
        let url = reqwest::Url::parse(&origin)?;
        if !address.ip().is_loopback()
            || url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || config_file.is_none()
        {
            return Err("local fixtures require explicit loopback endpoints and config".into());
        }
    } else {
        if bind != "0.0.0.0:8080"
            || origin != "http://gateway-bindings.internal"
            || config_file.is_some()
        {
            return Err("production endpoints are fixed".into());
        }
        #[cfg(unix)]
        if unsafe { libc::geteuid() } == 0 {
            return Err("native host requires non-root Linux".into());
        }
        #[cfg(not(target_os = "linux"))]
        return Err("native host requires Linux".into());
    }
    let source = if let Some(path) = config_file {
        ConfigSource::File(path)
    } else {
        ConfigSource::Environment(std::env::var("GATEWAY_DEMO_CONFIG_JSON")?)
    };
    source.load()?;
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let listener = TcpListener::bind(address)?;
    let host = if local {
        listener.local_addr()?.to_string()
    } else {
        "native-container.invalid".into()
    };
    let mut cache = projection::Cache::new(cache_enabled);
    println!(
        "Native Rust gateway listening at {} ({}, cache={})",
        listener.local_addr()?,
        if local {
            "quiescent synthetic disclosure fixture"
        } else {
            "requires current disclosure authority"
        },
        cache_enabled
    );
    for stream in listener.incoming() {
        let stream = stream?;
        let mut fallback = stream.try_clone()?;
        match transport::read_request(stream, &host) {
            Ok(mut request) => {
                if serve(&mut request, &source, &client, &origin, local, &mut cache).is_err() {
                    let _ = request.reply(403, b"View unavailable or access denied\n");
                }
            }
            Err(_) => {
                let _ = transport::reply_stream(&mut fallback, 400, b"Invalid request\n");
            }
        }
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        if std::env::args().any(|a| a == "--local-test" || a == "--local-window") {
            eprintln!("local fixture failure: {error}");
        }
        eprintln!("Native gateway unavailable; check configuration, authority and limits");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprint_changes_with_bytes() {
        assert_ne!(policy::digest(b"old"), policy::digest(b"new"));
    }

    #[test]
    fn fresh_process_can_construct_https_client_before_mode_dispatch() {
        const CHILD: &str = "HEDDLE_GATEWAY_TLS_STARTUP_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            configure_tls();
            reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("standalone gateway configures its TLS provider");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tests::fresh_process_can_construct_https_client_before_mode_dispatch",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("fresh test process");
        assert!(
            output.status.success(),
            "standalone TLS startup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
