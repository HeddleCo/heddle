use std::{io, sync::Arc, time::Duration};

use cli_shared::ClientConfig;
use rustls::{
    ClientConfig as RustlsClientConfig, RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    tungstenite::{
        Error,
        handshake::client::{Request, Response},
        http::header::HOST,
    },
};

type ConnectResult = Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), Error>;

/// Connect a WebSocket with the same TLS trust and server-name policy as hosted RPCs.
pub async fn connect_websocket(mut request: Request, config: &ClientConfig) -> ConnectResult {
    if config.tls_skip_verify {
        return Err(invalid_input(
            "TLS skip-verify is not supported for hosted WebSocket connections",
        ));
    }

    let (host, port) = prepare_request(&mut request, config)?;
    let connector = tls_connector(config)?;
    let duration = Duration::from_secs(config.timeout_secs.max(1));
    let socket = timeout(duration, TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| {
            Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "WebSocket connect timed out",
            ))
        })?
        .map_err(Error::Io)?;
    timeout(
        duration,
        client_async_tls_with_config(request, socket, None, connector),
    )
    .await
    .map_err(|_| {
        Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "WebSocket TLS handshake timed out",
        ))
    })?
}

fn prepare_request(request: &mut Request, config: &ClientConfig) -> Result<(String, u16), Error> {
    let original_host = request
        .uri()
        .host()
        .ok_or_else(|| invalid_input("WebSocket URL has no host"))?
        .to_string();
    let port = request
        .uri()
        .port_u16()
        .or_else(|| match request.uri().scheme_str() {
            Some("wss") => Some(443),
            Some("ws") => Some(80),
            _ => None,
        })
        .ok_or_else(|| invalid_input("WebSocket URL has no usable port"))?;

    if request.uri().scheme_str() == Some("wss")
        && let Some(server_name) = config.tls_domain_name.as_deref()
    {
        if server_name.is_empty() {
            return Err(invalid_input("TLS server-name override is empty"));
        }
        let original_authority = request
            .uri()
            .authority()
            .ok_or_else(|| invalid_input("WebSocket URL has no authority"))?
            .clone();
        let mut parts = request.uri().clone().into_parts();
        parts.authority = Some(websocket_authority(server_name, port).parse().map_err(
            |error| invalid_input(format!("invalid TLS server-name override: {error}")),
        )?);
        *request.uri_mut() = tokio_tungstenite::tungstenite::http::Uri::from_parts(parts)
            .map_err(|error| invalid_input(format!("invalid WebSocket URL: {error}")))?;
        request.headers_mut().insert(
            HOST,
            original_authority
                .as_str()
                .parse()
                .map_err(|error| invalid_input(format!("invalid WebSocket authority: {error}")))?,
        );
    }

    Ok((original_host, port))
}

fn websocket_authority(host: &str, port: u16) -> String {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{host}]:{port}"),
        Ok(std::net::IpAddr::V4(_)) | Err(_) => format!("{host}:{port}"),
    }
}

fn tls_connector(config: &ClientConfig) -> Result<Option<Connector>, Error> {
    let Some(ca_pem) = config.tls_ca_certificate_pem.as_deref() else {
        return Ok(None);
    };

    let certificates = CertificateDer::pem_slice_iter(ca_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid_input(format!("invalid TLS CA certificate bundle: {error}")))?;
    if certificates.is_empty() {
        return Err(invalid_input(
            "TLS CA certificate bundle contains no certificates",
        ));
    }

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|error| invalid_input(format!("invalid TLS CA certificate: {error}")))?;
    }
    let tls = RustlsClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(Connector::Rustls(Arc::new(tls))))
}

fn invalid_input(message: impl Into<String>) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

#[cfg(test)]
mod tests {
    use cli_shared::ClientConfig;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::header::HOST};

    use super::{prepare_request, tls_connector};

    #[test]
    fn server_name_override_preserves_dial_target_and_http_authority() {
        let mut request = "wss://127.0.0.1:8421/presence/ws"
            .into_client_request()
            .unwrap();
        let config = ClientConfig::default().with_tls_domain_name("localhost");

        let dial_target = prepare_request(&mut request, &config).unwrap();

        assert_eq!(dial_target, ("127.0.0.1".to_string(), 8421));
        assert_eq!(
            request.uri().to_string(),
            "wss://localhost:8421/presence/ws"
        );
        assert_eq!(request.headers()[HOST], "127.0.0.1:8421");
    }

    #[test]
    fn configured_ca_bundle_is_parsed_before_connecting() {
        let config = ClientConfig::default().with_tls_ca_certificate_pem("not a certificate");

        let error = match tls_connector(&config) {
            Ok(_) => panic!("invalid CA bundle should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("certificate"));
    }
}
