// SPDX-License-Identifier: Apache-2.0
//! Pure redaction display helpers (no crypto/store I/O).

/// Short-form display for a hex-encoded public key (first 16 chars + ellipsis).
pub fn short_public_key(hex: &str) -> String {
    if hex.len() <= 16 {
        hex.to_string()
    } else {
        format!("{}…", &hex[..16])
    }
}

/// Signature verification status for redaction records.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RedactionSignatureStatus {
    Unsigned,
    Verified,
    Tampered,
}

impl RedactionSignatureStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unsigned => "unsigned",
            Self::Verified => "verified",
            Self::Tampered => "tampered",
        }
    }
}

/// Map verify outcomes into redaction signature status.
///
/// Mirrors historical CLI mapping: missing signature → unsigned; verify
/// success → verified; verify error → tampered; verify false treated as
/// unsigned (unreachable in practice at the CLI boundary).
pub fn redaction_signature_status(
    has_signature: bool,
    verified: Result<bool, ()>,
) -> RedactionSignatureStatus {
    match (has_signature, verified) {
        (false, _) => RedactionSignatureStatus::Unsigned,
        (true, Ok(true)) => RedactionSignatureStatus::Verified,
        (true, Ok(false)) => RedactionSignatureStatus::Unsigned,
        (true, Err(())) => RedactionSignatureStatus::Tampered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_key_and_status() {
        assert_eq!(short_public_key("abcd"), "abcd");
        assert_eq!(
            short_public_key("0123456789abcdef0123"),
            "0123456789abcdef…"
        );
        assert_eq!(
            redaction_signature_status(false, Err(())).label(),
            "unsigned"
        );
        assert_eq!(
            redaction_signature_status(true, Ok(true)).label(),
            "verified"
        );
        assert_eq!(
            redaction_signature_status(true, Ok(false)).label(),
            "unsigned"
        );
        assert_eq!(
            redaction_signature_status(true, Err(())).label(),
            "tampered"
        );
    }
}
