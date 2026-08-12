// SPDX-License-Identifier: Apache-2.0

use crate::store::Result;

/// Compress one compact metadata frame with the measured lineage-solid policy.
///
/// Level 19 plus a 2^27-byte long-distance window matches the #1325 falsifier.
/// Incompressible input remains raw so the pack reader's size discriminator is
/// unambiguous. Builds without `zstd` retain the lossless compact encoding but
/// store its frames raw.
pub fn compress_compact_frame(data: &[u8]) -> Result<Vec<u8>> {
    #[cfg(feature = "zstd")]
    {
        use std::io::Write;

        let mut compressed = Vec::new();
        let mut encoder = zstd::stream::write::Encoder::new(&mut compressed, 19)?;
        encoder.window_log(27)?;
        encoder.long_distance_matching(true)?;
        encoder.include_checksum(true)?;
        encoder.set_pledged_src_size(Some(data.len() as u64))?;
        encoder.write_all(data)?;
        encoder.finish()?;
        if compressed.len() < data.len() {
            return Ok(compressed);
        }
    }
    Ok(data.to_vec())
}

#[cfg(all(test, feature = "zstd"))]
mod tests {
    use super::*;
    use crate::store::pack::{decompress_pack_payload, has_zstd_magic};

    #[test]
    fn solid_compression_round_trips_and_carries_a_zstd_checksum() {
        let input = b"directory version\n".repeat(32_768);
        let compressed = compress_compact_frame(&input).unwrap();
        assert!(has_zstd_magic(&compressed));
        assert!(compressed.len() < input.len());
        assert_eq!(
            decompress_pack_payload(&compressed, input.len()).unwrap(),
            input
        );
    }
}
