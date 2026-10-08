// SPDX-License-Identifier: Apache-2.0
//! Reversible UTF-8 names for portable, case-sensitive storage.

use std::path::{Path, PathBuf};

/// Remove only one LF or CRLF framing terminator, preserving name bytes.
pub fn strip_ref_line_ending(value: &str) -> &str {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value)
}

/// Percent escaping shared by ref storage and managed checkouts. Uppercase
/// ASCII is escaped too: distinct names stay distinct on case-folding hosts.
/// The `n-` prefix excludes Windows device names; dots and hostile bytes escape.
pub fn encode_name(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::from("n-");
    for &byte in value.as_bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 15) as usize] as char);
        }
    }
    out
}

/// Strict inverse; reject aliases and malformed/non-UTF-8 encodings.
pub fn decode_name(encoded: &str) -> Option<String> {
    let bytes = encoded.strip_prefix("n-")?.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = (*bytes.get(index + 1)? as char).to_digit(16)?;
            let lo = (*bytes.get(index + 2)? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    let value = String::from_utf8(out).ok()?;
    (encode_name(&value) == encoded).then_some(value)
}

/// Maximum relative name path in bytes. With a 512-byte repository root,
/// `.heddle/threads/` (17), a 255-byte checkout leaf plus separator (256),
/// and 111 bytes for checkout-local Heddle metadata, the absolute path is
/// at most 1024 bytes. Two encoded names in remote refs also fit this budget.
pub const NAME_PATH_BUDGET: usize = 128;

/// Bounded components and a terminal directory keep even long names disjoint.
/// Each chunk is ASCII, at most 182 bytes; `entry` can never be another chunk.
/// Long names use the full BLAKE3 digest of the exact native UTF-8 identity.
/// Their entry must carry a `name` file, verified by the filesystem reader.
pub fn name_path(value: &str) -> PathBuf {
    let source = git_name(value);
    let encoded = encode_name(&source);
    let mut path = PathBuf::new();
    if matches!(source, std::borrow::Cow::Owned(_)) {
        // Encode the original only once, keeping maximum-length imported
        // names below total path limits as well as component limits.
        path.push("git");
    }
    for chunk in encoded.as_bytes()[2..].chunks(180) {
        // The encoding is ASCII; collecting bytes avoids unchecked decoding.
        let chunk: String = chunk.iter().map(|byte| *byte as char).collect();
        path.push(format!("n-{chunk}"));
    }
    path.push("entry");
    if path.as_os_str().len() > NAME_PATH_BUDGET {
        PathBuf::from(format!(
            "h-{}/entry",
            blake3::hash(value.as_bytes()).to_hex()
        ))
    } else {
        path
    }
}

/// Recognize the complete digest path; never accept a digest prefix or alias.
pub fn is_digest_name_path(path: &Path) -> bool {
    let mut parts = path.components();
    let Some(digest) = parts.next().and_then(|part| part.as_os_str().to_str()) else {
        return false;
    };
    digest.strip_prefix("h-").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) && parts.next().is_some_and(|part| part.as_os_str() == "entry")
        && parts.next().is_none()
}

/// Decode only canonical storage paths, including exact UTF-8 identity.
pub fn decode_name_path(path: &Path) -> Option<String> {
    let mut parts = path.components().peekable();
    let imported = parts.peek().is_some_and(|part| part.as_os_str() == "git");
    if imported {
        parts.next();
    }
    let mut encoded = String::from("n-");
    while let Some(part) = parts.next() {
        let part = part.as_os_str().to_str()?;
        if parts.peek().is_none() {
            if part != "entry" {
                return None;
            }
        } else {
            encoded.push_str(part.strip_prefix("n-")?);
        }
    }
    let source = decode_name(&encoded)?;
    let value = if imported {
        native_git_name(&source)
    } else {
        source
    };
    (name_path(&value) == path).then_some(value)
}

/// Map Git names that collide with native reservation (or the escape prefix)
/// into a distinct native identity. Signed source refs always keep their bytes.
pub fn native_git_name(name: &str) -> String {
    if crate::object::is_reserved_heddle_namespace(name) || name.starts_with("git%") {
        format!("git%{}", encode_name(name))
    } else {
        name.to_owned()
    }
}

/// Undo the reserved import mapping when projecting a native name back to Git.
pub fn git_name(native: &str) -> std::borrow::Cow<'_, str> {
    match native.strip_prefix("git%").and_then(decode_name) {
        Some(name) if native_git_name(&name) == native => std::borrow::Cow::Owned(name),
        _ => std::borrow::Cow::Borrowed(native),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_names_are_exact_bounded_and_case_distinct() {
        for name in [
            "CON",
            "con",
            "a,b",
            "x'$(true)",
            "a%2F",
            "ünicode/ブランチ",
            "trailing\u{a0}",
            "literal\u{fffd}",
            &"界".repeat(337),
            &native_git_name(&format!("heddle/{}", "界".repeat(333))),
        ] {
            let path = name_path(name);
            if is_digest_name_path(&path) {
                assert!(decode_name_path(&path).is_none());
            } else {
                assert_eq!(decode_name_path(&path).as_deref(), Some(name));
            }
            assert!(path.components().all(|part| part.as_os_str().len() <= 182));
            assert!(path.to_str().expect("ASCII path").is_ascii());
            assert!(path.as_os_str().len() <= NAME_PATH_BUDGET);
        }
        assert_ne!(
            name_path("CON").to_string_lossy().to_lowercase(),
            name_path("con").to_string_lossy().to_lowercase()
        );
        let short = "x".repeat(178);
        assert!(!name_path(&format!("{short}y")).starts_with(name_path(&short)));
    }

    #[test]
    fn reserved_git_mapping_does_not_alias_literal_escape_names() {
        for name in ["heddle/foo", "Heddle/foo", "git%n-heddle%2Ffoo", "a%2Fb"] {
            let native = native_git_name(name);
            assert!(!crate::object::is_reserved_heddle_namespace(&native));
            assert_eq!(git_name(&native), name);
        }
        assert_ne!(
            native_git_name("heddle/foo"),
            native_git_name("git%n-heddle%2Ffoo")
        );
    }
}
