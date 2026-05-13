// SPDX-License-Identifier: Apache-2.0
//! Bloom filter for changed-path queries.

const BLOOM_KEY: &[u8; 32] = b"heddle-changed-path-bloom-v1\0\0\0\0";
const BLOOM_BITS: usize = 256 * 8; // 2048

pub(crate) fn bloom_insert(bits: &mut [u8; 256], path: &str) {
    let mut p = path;
    loop {
        for pos in bloom_positions(p) {
            bits[pos / 8] |= 1 << (pos % 8);
        }
        match p.rfind('/') {
            Some(idx) => p = &p[..idx],
            None => break,
        }
    }
}

pub(crate) fn bloom_maybe_contains(bits: &[u8; 256], path: &str) -> bool {
    bloom_positions(path)
        .iter()
        .all(|&pos| bits[pos / 8] & (1 << (pos % 8)) != 0)
}

fn bloom_positions(path: &str) -> [usize; 3] {
    let hash = blake3::keyed_hash(BLOOM_KEY, path.as_bytes());
    let b = hash.as_bytes();
    [
        u16::from_le_bytes([b[0], b[1]]) as usize % BLOOM_BITS,
        u16::from_le_bytes([b[2], b[3]]) as usize % BLOOM_BITS,
        u16::from_le_bytes([b[4], b[5]]) as usize % BLOOM_BITS,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_insert_and_contains() {
        let mut bits = [0u8; 256];
        bloom_insert(&mut bits, "src/lib.rs");
        assert!(bloom_maybe_contains(&bits, "src/lib.rs"));
        assert!(bloom_maybe_contains(&bits, "src")); // prefix inserted
    }

    #[test]
    fn bloom_definitely_not_contains() {
        let bits = [0u8; 256]; // empty bloom
        assert!(!bloom_maybe_contains(&bits, "anything"));
    }

    #[test]
    fn bloom_prefix_insertion() {
        let mut bits = [0u8; 256];
        bloom_insert(&mut bits, "src/foo/bar.rs");
        // All prefixes should be findable
        assert!(bloom_maybe_contains(&bits, "src/foo/bar.rs"));
        assert!(bloom_maybe_contains(&bits, "src/foo"));
        assert!(bloom_maybe_contains(&bits, "src"));
    }

    #[test]
    fn bloom_false_positive_rate() {
        let mut bits = [0u8; 256];
        let inserted: Vec<String> = (0..10).map(|i| format!("src/file{i}.rs")).collect();
        for path in &inserted {
            bloom_insert(&mut bits, path);
        }
        // Inserted paths must be found
        for path in &inserted {
            assert!(bloom_maybe_contains(&bits, path));
        }
        // Count false positives among random non-inserted paths
        let mut false_positives = 0;
        for i in 100..1100 {
            let path = format!("other/path{i}.rs");
            if bloom_maybe_contains(&bits, &path) {
                false_positives += 1;
            }
        }
        // FPR should be well below 10%
        assert!(
            false_positives < 100,
            "Too many false positives: {false_positives}"
        );
    }
}