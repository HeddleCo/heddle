//! Safe, bounded text for the device run feed.

/// Redact one excerpt before it becomes an authorized timeline projection.
/// Callers select only command, path, or message fields, never tool results or
/// file-content fields.
pub(crate) fn redact_excerpt(raw: &str, limit: usize) -> String {
    if raw.to_ascii_lowercase().contains("authorization") {
        return "[sensitive text redacted]".into();
    }
    let mut words = raw.split_whitespace().peekable();
    let mut output = Vec::new();
    let mut leading = true;
    while let Some(word) = words.next() {
        let lower = word.to_ascii_lowercase();
        if leading && (word == "env" || word == "export") {
            continue;
        }
        if word.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            if let Some((_, value)) = word.split_once('=') {
                for quote in ['\'', '"'] {
                    if value.matches(quote).count() % 2 == 1 {
                        for part in words.by_ref() {
                            if part.contains(quote) {
                                break;
                            }
                        }
                    }
                }
            }
            continue;
        }
        leading = false;
        if word.contains("<<") || matches!(word, "echo" | "printf") || word == "-c" {
            output.push("[content redacted]".to_string());
            break;
        }
        if lower.starts_with("--")
            && ["password", "passwd", "token", "secret", "api-key", "apikey"]
                .iter()
                .any(|part| lower.contains(part))
        {
            let flag = word.split('=').next().unwrap_or(word);
            output.push(if word.contains('=') {
                format!("{flag}=[redacted]")
            } else {
                format!("{flag} [redacted]")
            });
            break;
        }
        if [
            "token=",
            "password=",
            "passwd=",
            "api_key=",
            "apikey=",
            "secret=",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            output.push("[redacted]".to_string());
            continue;
        }
        if lower == "bearer" {
            output.push("Bearer [redacted]".to_string());
            let _ = words.next();
            continue;
        }
        let token = lower.trim_start_matches(['\'', '"']);
        if token.starts_with("ghp_") || token.starts_with("sk-") || token.starts_with("xoxb-") {
            output.push("[redacted]".to_string());
            continue;
        }
        output.push(word.to_string());
    }
    let joined = output.join(" ");
    if joined.chars().count() <= limit {
        return joined;
    }
    let prefix: String = joined.chars().take(limit.saturating_sub(1)).collect();
    format!("{prefix}…")
}

#[cfg(test)]
mod tests {
    use super::redact_excerpt;

    #[test]
    fn redaction_covers_commands_messages_and_content() {
        let cases = [
            ("FOO=bar cargo test -p weft-auth", "cargo test -p weft-auth"),
            ("env FOO=bar TOKEN=hidden cargo check", "cargo check"),
            ("foo=hidden cargo check", "cargo check"),
            (
                "curl --password hunter2 --token=abc",
                "curl --password [redacted]",
            ),
            ("curl --token=abc", "curl --token=[redacted]"),
            ("curl --access-token=abc", "curl --access-token=[redacted]"),
            ("curl --api-key hidden", "curl --api-key [redacted]"),
            (
                "curl Authorization: Bearer secret",
                "[sensitive text redacted]",
            ),
            ("env FOO='hidden value' cargo check", "cargo check"),
            (
                "run ghp_abcdef sk-secret xoxb-token",
                "run [redacted] [redacted] [redacted]",
            ),
            ("run 'ghp_abcdef'", "run [redacted]"),
            ("cargo test\nSECRET=hidden", "cargo test"),
            ("FOO=bar\ncargo   test", "cargo test"),
            ("cat <<EOF secret file contents", "cat [content redacted]"),
            ("echo secret file contents", "[content redacted]"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_excerpt(input, 120), expected, "{input}");
        }
        assert_eq!(redact_excerpt(&"x".repeat(130), 120).chars().count(), 120);
        assert!(redact_excerpt(&"x".repeat(130), 120).ends_with('…'));
    }
}
