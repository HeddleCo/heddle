// SPDX-License-Identifier: Apache-2.0
//! Shared TLS-trust failure wording for hosted bootstrap and Git HTTPS.

/// Operator-facing CA setting named by every TLS trust failure.
pub const REMOTE_TLS_CA_CERT_SETTING: &str = "HEDDLE_REMOTE_TLS_CA_CERT";

/// True when a transport error is a peer-certificate trust failure.
pub fn is_tls_trust_failure(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("invalid peer certificate")
        || normalized.contains("unknownissuer")
        || normalized.contains("unknown issuer")
        || normalized.contains("certificateunknown")
}

/// Append the CA configuration that fixes an untrusted peer certificate.
pub fn annotate_tls_trust_failure(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    if is_tls_trust_failure(&message) && !message.contains(REMOTE_TLS_CA_CERT_SETTING) {
        format!(
            "{message}; trust this server's CA with {REMOTE_TLS_CA_CERT_SETTING}=/path/to/ca.pem"
        )
    } else {
        message
    }
}

/// Walk an error source chain and annotate a TLS trust failure if any link is one.
pub fn annotate_error_chain_tls_trust_failure(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut current = error.source();
    let mut trust_failure = is_tls_trust_failure(&message);
    while let Some(err) = current {
        let next = err.to_string();
        trust_failure |= is_tls_trust_failure(&next);
        if !message.contains(&next) {
            message.push_str(": ");
            message.push_str(&next);
        }
        current = err.source();
    }
    if trust_failure {
        annotate_tls_trust_failure(message)
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::{REMOTE_TLS_CA_CERT_SETTING, annotate_tls_trust_failure, is_tls_trust_failure};

    #[test]
    fn unknown_issuer_is_a_tls_trust_failure() {
        assert!(is_tls_trust_failure(
            "invalid peer certificate: UnknownIssuer"
        ));
        assert!(!is_tls_trust_failure("connection refused"));
    }

    #[test]
    fn annotation_names_the_ca_setting_once() {
        let annotated = annotate_tls_trust_failure("invalid peer certificate: UnknownIssuer");
        assert!(annotated.contains(REMOTE_TLS_CA_CERT_SETTING));
        assert_eq!(
            annotate_tls_trust_failure(annotated.as_str()),
            annotated,
            "already-annotated messages must stay stable"
        );
    }

    #[test]
    fn annotation_walks_a_source_chain() {
        #[derive(Debug)]
        struct Root(&'static str);
        impl std::fmt::Display for Root {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Root {}

        #[derive(Debug)]
        struct Wrapper {
            source: Root,
        }
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.source)
            }
        }

        let error = Wrapper {
            source: Root("invalid peer certificate: UnknownIssuer"),
        };
        let annotated = super::annotate_error_chain_tls_trust_failure(&error);
        assert!(annotated.contains("error sending request"));
        assert!(annotated.contains("UnknownIssuer"));
        assert!(annotated.contains(REMOTE_TLS_CA_CERT_SETTING));
    }
}
