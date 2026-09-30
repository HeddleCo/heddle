// SPDX-License-Identifier: Apache-2.0
//! Web approval host for browser pairing (`heddle auth login --host`).
//!
//! This is the host of the web page a human opens to approve a CLI pairing,
//! not the Heddle API server (`--server`). The value travels as
//! `BeginPairingRequest.web_origin`. The server accepts it only when it matches
//! its own CORS allowlist and preview-origin policy, so the local check is a
//! shape check: a canonical `https://` plus lowercase DNS host, with nothing
//! else. It grants nothing and is not a secret.

use std::{fmt, str::FromStr};

const HTTPS_PREFIX: &str = "https://";
const MAX_HOST_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// A canonical `https://<lowercase DNS host>` web approval origin.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PairingWebOrigin(String);

/// Why a `--host` value is not a web approval host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WebHostError {
    #[error("the web host is empty")]
    Empty,
    #[error("the web host must use https, not `{scheme}`")]
    NotHttps { scheme: String },
    #[error("the web host must not include userinfo (`user@`)")]
    Userinfo,
    #[error("the web host must not include a port")]
    Port,
    #[error("the web host must not include a path")]
    Path,
    #[error("the web host must not include a query or fragment")]
    QueryOrFragment,
    #[error("the web host must not contain a wildcard")]
    Wildcard,
    #[error("`{host}` is not a DNS host name")]
    NotDnsHost { host: String },
}

impl PairingWebOrigin {
    /// Accept a bare host (`preview.example.dev`) or an `https://` origin, and
    /// normalise it to `https://<lowercase host>`. A single trailing `/` is
    /// the empty path and is accepted; anything else beyond the host is not.
    pub fn parse(input: &str) -> Result<Self, WebHostError> {
        let rest = match input.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => rest,
            Some((scheme, _)) => {
                return Err(WebHostError::NotHttps {
                    scheme: scheme.to_ascii_lowercase(),
                });
            }
            None => input,
        };
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, suffix) = rest.split_at(authority_end);
        if authority.contains('@') {
            return Err(WebHostError::Userinfo);
        }
        if authority.contains('*') {
            return Err(WebHostError::Wildcard);
        }
        if authority.contains('[') || authority.contains(']') {
            return Err(WebHostError::NotDnsHost {
                host: authority.to_string(),
            });
        }
        if authority.contains(':') {
            return Err(WebHostError::Port);
        }
        match suffix {
            "" | "/" => {}
            _ if suffix.starts_with(['?', '#'])
                || suffix.starts_with("/?")
                || suffix.starts_with("/#") =>
            {
                return Err(WebHostError::QueryOrFragment);
            }
            _ => return Err(WebHostError::Path),
        }
        if authority.is_empty() {
            return Err(WebHostError::Empty);
        }
        let host = authority.to_ascii_lowercase();
        if !is_dns_host(&host) {
            return Err(WebHostError::NotDnsHost { host });
        }
        Ok(Self(format!("{HTTPS_PREFIX}{host}")))
    }

    /// The canonical origin, e.g. `https://preview.example.dev`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PairingWebOrigin {
    type Err = WebHostError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

impl fmt::Display for PairingWebOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A lowercase multi-label DNS name: LDH labels of 1..=63 bytes, no leading or
/// trailing hyphen, at most 253 bytes, and a non-numeric final label (so an
/// IPv4 literal is not a host name).
fn is_dns_host(host: &str) -> bool {
    if host.len() > MAX_HOST_LEN {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let labels_valid = labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    });
    labels_valid
        && labels
            .last()
            .is_some_and(|tld| !tld.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &str) -> String {
        PairingWebOrigin::parse(input)
            .unwrap_or_else(|error| panic!("{input} should parse: {error}"))
            .to_string()
    }

    #[test]
    fn bare_hosts_and_https_origins_normalise_to_one_canonical_origin() {
        let canonical = "https://pr-17-tapestry.zephyr-forge.workers.dev";
        for input in [
            "pr-17-tapestry.zephyr-forge.workers.dev",
            "PR-17-Tapestry.Zephyr-Forge.Workers.Dev",
            "https://pr-17-tapestry.zephyr-forge.workers.dev",
            "HTTPS://pr-17-tapestry.zephyr-forge.workers.dev",
            "https://pr-17-tapestry.zephyr-forge.workers.dev/",
            "pr-17-tapestry.zephyr-forge.workers.dev/",
        ] {
            assert_eq!(parsed(input), canonical, "{input}");
        }
        assert_eq!(parsed("heddle.sh"), "https://heddle.sh");
        assert_eq!(
            parsed("xn--bcher-kva.example"),
            "https://xn--bcher-kva.example"
        );
    }

    #[test]
    fn non_origin_shapes_are_rejected_with_a_typed_reason() {
        let cases: &[(&str, WebHostError)] = &[
            ("", WebHostError::Empty),
            ("https://", WebHostError::Empty),
            ("https:///", WebHostError::Empty),
            (
                "http://preview.example.dev",
                WebHostError::NotHttps {
                    scheme: "http".into(),
                },
            ),
            (
                "HTTP://preview.example.dev",
                WebHostError::NotHttps {
                    scheme: "http".into(),
                },
            ),
            (
                "ftp://preview.example.dev",
                WebHostError::NotHttps {
                    scheme: "ftp".into(),
                },
            ),
            ("https://preview.example.dev:8443", WebHostError::Port),
            ("preview.example.dev:443", WebHostError::Port),
            (
                "https://preview.example.dev/auth/device",
                WebHostError::Path,
            ),
            ("preview.example.dev/app", WebHostError::Path),
            ("https://user@preview.example.dev", WebHostError::Userinfo),
            (
                "https://user:pw@preview.example.dev",
                WebHostError::Userinfo,
            ),
            (
                "https://preview.example.dev?next=1",
                WebHostError::QueryOrFragment,
            ),
            (
                "https://preview.example.dev/?next=1",
                WebHostError::QueryOrFragment,
            ),
            (
                "https://preview.example.dev#top",
                WebHostError::QueryOrFragment,
            ),
            ("https://*.example.dev", WebHostError::Wildcard),
            ("*.example.dev", WebHostError::Wildcard),
        ];
        for (input, expected) in cases {
            assert_eq!(
                PairingWebOrigin::parse(input).as_ref(),
                Err(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn non_dns_hosts_are_rejected() {
        let long_label = format!("{}.example.dev", "a".repeat(64));
        let long_host = format!("{}.dev", ["abcdefghij"; 25].join("."));
        for input in [
            "localhost",
            "127.0.0.1",
            "https://10.0.0.1",
            "https://[::1]",
            "preview..example.dev",
            ".example.dev",
            "example.dev.",
            "-preview.example.dev",
            "preview-.example.dev",
            "pre_view.example.dev",
            "pre view.example.dev",
            " preview.example.dev",
            "bücher.example",
            long_label.as_str(),
            long_host.as_str(),
        ] {
            assert!(
                matches!(
                    PairingWebOrigin::parse(input),
                    Err(WebHostError::NotDnsHost { .. })
                ),
                "{input:?} must be rejected as a non-DNS host, got {:?}",
                PairingWebOrigin::parse(input)
            );
        }
    }

    #[test]
    fn rejection_messages_name_the_problem() {
        let error = PairingWebOrigin::parse("http://preview.example.dev")
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            error.as_deref(),
            Some("the web host must use https, not `http`")
        );
    }
}
