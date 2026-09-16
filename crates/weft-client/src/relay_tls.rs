// SPDX-License-Identifier: Apache-2.0
//! Relay HTTPS trust from the same PEM unary HTTPS already uses.
//!
//! rustls will store a leaf as a trust anchor, then reject it during path
//! building (`UnknownIssuer`). Exact-DER pinning covers that case; WebPKI
//! with Mozilla roots plus the same extra certs still covers a real CA PEM.

use std::{io, sync::Arc};

use anyhow::{Context, Result, bail};
use iroh::tls::CaTlsConfig;
use rustls::{
    DigitallySignedStruct, Error as TlsError, SignatureScheme,
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_name,
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject},
    server::ParsedCertificate,
};

pub(crate) fn ca_tls_config_from_pem(pem: &str) -> Result<CaTlsConfig> {
    let certificates = parse_pem_certificates(pem)?;
    Ok(CaTlsConfig::custom_server_cert_verifier(Arc::new(
        move |crypto_provider| {
            Ok(Arc::new(PinnedServerCertVerifier::new(
                certificates.clone(),
                crypto_provider,
            )?))
        },
    )))
}

fn parse_pem_certificates(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certificates = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse TLS CA certificate PEM")?;
    if certificates.is_empty() {
        bail!("TLS CA certificate bundle contains no certificates");
    }
    Ok(certificates)
}

#[derive(Debug)]
struct PinnedServerCertVerifier {
    pinned: Vec<CertificateDer<'static>>,
    inner: Arc<dyn ServerCertVerifier>,
}

impl PinnedServerCertVerifier {
    fn new(
        pinned: Vec<CertificateDer<'static>>,
        crypto_provider: Arc<CryptoProvider>,
    ) -> io::Result<Self> {
        let inner = CaTlsConfig::embedded()
            .with_extra_roots(pinned.clone())
            .server_cert_verifier(crypto_provider)?;
        Ok(Self { pinned, inner })
    }
}

impl ServerCertVerifier for PinnedServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if self
            .pinned
            .iter()
            .any(|certificate| certificate.as_ref() == end_entity.as_ref())
        {
            let parsed = ParsedCertificate::try_from(end_entity)?;
            verify_server_name(&parsed, server_name)?;
            return Ok(ServerCertVerified::assertion());
        }
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use iroh::{
        Endpoint, RelayMode,
        endpoint::presets,
        tls::{CaTlsConfig, default_provider},
    };
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{
        ClientConnection, ServerConfig, ServerConnection,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    };

    use super::{ca_tls_config_from_pem, parse_pem_certificates};

    #[test]
    fn garbage_pem_fails_closed() {
        let error = parse_pem_certificates("not a certificate").expect_err("garbage PEM");
        assert!(
            error
                .to_string()
                .contains("TLS CA certificate bundle contains no certificates"),
            "unexpected error: {error:#}"
        );
        let error = parse_pem_certificates(
            "-----BEGIN CERTIFICATE-----\nnot-valid-base64\n-----END CERTIFICATE-----\n",
        )
        .expect_err("malformed PEM");
        assert!(
            error.to_string().contains("parse TLS CA certificate PEM"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn empty_bundle_fails_closed() {
        let error = parse_pem_certificates("").expect_err("empty PEM");
        assert!(
            error
                .to_string()
                .contains("TLS CA certificate bundle contains no certificates"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn handshake_accepts_a_server_presenting_the_configured_leaf() {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".to_string()]).expect("test cert");
        let pem = cert.pem();
        let server = server_config(cert.der().clone(), signing_key.serialize_der());
        let trusted = ca_tls_config_from_pem(&pem)
            .expect("parse leaf PEM")
            .client_config(default_provider())
            .expect("client TLS config from pinned leaf");

        handshake(trusted, server.clone()).expect("pinned leaf must complete the handshake");

        let untrusted = CaTlsConfig::embedded()
            .client_config(default_provider())
            .expect("default Mozilla roots");
        handshake(untrusted, server).expect_err("Mozilla roots must reject the private leaf");
    }

    #[tokio::test]
    async fn endpoint_binds_with_the_configured_pem() {
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(vec!["127.0.0.1".to_string()]).expect("test cert");
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .ca_tls_config(ca_tls_config_from_pem(&cert.pem()).expect("parse leaf PEM"))
            .bind()
            .await
            .expect("bind Endpoint with the same PEM unary HTTPS uses");
        endpoint.close().await;
    }

    fn server_config(cert_der: CertificateDer<'static>, key_der: Vec<u8>) -> ServerConfig {
        ServerConfig::builder_with_provider(default_provider())
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![cert_der],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
            )
            .expect("server TLS config")
    }

    fn handshake(client: rustls::ClientConfig, server: ServerConfig) -> Result<(), rustls::Error> {
        let name = ServerName::try_from("127.0.0.1").expect("loopback server name");
        let mut client = ClientConnection::new(Arc::new(client), name)?;
        let mut server = ServerConnection::new(Arc::new(server))?;
        let mut to_server = Vec::new();
        let mut to_client = Vec::new();
        for _ in 0..32 {
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
            if client.wants_write() {
                client.write_tls(&mut to_server).map_err(io_to_tls)?;
            }
            if !to_server.is_empty() {
                let mut cursor = std::io::Cursor::new(&to_server);
                server.read_tls(&mut cursor).map_err(io_to_tls)?;
                to_server.clear();
                server.process_new_packets()?;
            }
            if server.wants_write() {
                server.write_tls(&mut to_client).map_err(io_to_tls)?;
            }
            if !to_client.is_empty() {
                let mut cursor = std::io::Cursor::new(&to_client);
                client.read_tls(&mut cursor).map_err(io_to_tls)?;
                to_client.clear();
                client.process_new_packets()?;
            }
        }
        Err(rustls::Error::General(
            "TLS handshake did not complete".to_string(),
        ))
    }

    fn io_to_tls(error: std::io::Error) -> rustls::Error {
        rustls::Error::General(error.to_string())
    }
}
