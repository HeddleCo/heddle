// SPDX-License-Identifier: Apache-2.0

use crate::store::Result;

/// zstd parameters for one compact metadata frame.
///
/// Decoding never consults these: a frame carries its own zstd framing, so a
/// store written at any level stays readable. Only the encoder's CPU/bytes
/// trade-off moves. The 2^27-byte long-distance window matches the #1325
/// falsifier at every level.
#[derive(Clone, Copy, Debug)]
pub struct CompactFrameCompression {
    pub level: i32,
    pub window_log: u32,
    pub long_distance_matching: bool,
}

impl CompactFrameCompression {
    /// Local repack policy: retain the pre-adoption level 19 encoder.
    pub const DEFAULT: Self = Self {
        level: 19,
        window_log: 27,
        long_distance_matching: true,
    };
    /// Fast zstd for adoption/import packs only (owner decision 2026-10-07).
    /// Weft adoption_pack must pass this to `compress_compact_frame_with`.
    pub const ADOPTION: Self = Self {
        level: 3,
        window_log: 27,
        long_distance_matching: true,
    };
}

impl Default for CompactFrameCompression {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Compress one compact metadata frame with [`CompactFrameCompression::DEFAULT`].
///
/// Incompressible input remains raw so the pack reader's size discriminator is
/// unambiguous. Builds without `zstd` retain the lossless compact encoding but
/// store its frames raw.
pub fn compress_compact_frame(data: &[u8]) -> Result<Vec<u8>> {
    compress_compact_frame_with(data, CompactFrameCompression::DEFAULT)
}

/// Compress one compact metadata frame with explicit zstd parameters. The
/// raw-when-incompressible rule and the `zstd`-less fallback are unchanged.
pub fn compress_compact_frame_with(
    data: &[u8],
    options: CompactFrameCompression,
) -> Result<Vec<u8>> {
    #[cfg(feature = "zstd")]
    {
        use std::io::Write;

        let mut compressed = Vec::new();
        let mut encoder = zstd::stream::write::Encoder::new(&mut compressed, options.level)?;
        encoder.window_log(options.window_log)?;
        encoder.long_distance_matching(options.long_distance_matching)?;
        encoder.include_checksum(true)?;
        encoder.set_pledged_src_size(Some(data.len() as u64))?;
        encoder.write_all(data)?;
        encoder.finish()?;
        if compressed.len() < data.len() {
            return Ok(compressed);
        }
    }
    #[cfg(not(feature = "zstd"))]
    {
        let _ = options;
    }
    Ok(data.to_vec())
}

#[cfg(all(test, feature = "zstd"))]
mod tests {
    use super::*;
    use crate::store::pack::{decompress_pack_payload, has_zstd_magic};

    fn lineage_input() -> Vec<u8> {
        // Repetitive like a State/tree lineage frame, with enough variation
        // that level 19 and level 3 produce different byte streams.
        (0..32_768u32)
            .flat_map(|i| format!("directory version {}\n", i % 97).into_bytes())
            .collect()
    }

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

    #[test]
    fn local_default_is_level_nineteen_and_adoption_is_level_three() {
        let options = CompactFrameCompression::DEFAULT;
        let adoption = CompactFrameCompression::ADOPTION;
        assert_eq!(options.level, 19);
        assert_eq!(adoption.level, 3);
        assert_eq!(adoption.window_log, 27);
        assert!(adoption.long_distance_matching);
        assert_eq!(options.window_log, 27);
        assert!(options.long_distance_matching);
        let input = lineage_input();
        assert_eq!(
            compress_compact_frame(&input).unwrap(),
            compress_compact_frame_with(&input, CompactFrameCompression::DEFAULT).unwrap(),
            "the plain entry point is exactly the DEFAULT policy"
        );
    }

    #[test]
    fn pre_change_level_nineteen_fixture_and_adoption_frames_decode() {
        let input = lineage_input();
        // Encoded once by the unmodified encoder from 660f99cbe9360b6f
        // (level 19, window_log 27, LDM, checksum and pledged input size).
        let solid = include_bytes!("../../../tests/fixtures/compact-level19.zstd");
        let fast = compress_compact_frame_with(&input, CompactFrameCompression::ADOPTION).unwrap();
        assert!(has_zstd_magic(solid) && has_zstd_magic(&fast));
        assert_ne!(
            solid.as_slice(),
            fast.as_slice(),
            "the two levels must exercise different encoders"
        );
        // A store written at level 19 before the policy change decodes with
        // the same reader as a fast frame; decoding carries no level.
        assert_eq!(decompress_pack_payload(solid, input.len()).unwrap(), input);
        assert_eq!(decompress_pack_payload(&fast, input.len()).unwrap(), input);
    }

    #[test]
    fn explicit_options_change_the_encoder() {
        let input = lineage_input();
        let no_ldm = compress_compact_frame_with(
            &input,
            CompactFrameCompression {
                level: 1,
                window_log: 20,
                long_distance_matching: false,
            },
        )
        .unwrap();
        assert!(has_zstd_magic(&no_ldm));
        assert_eq!(
            decompress_pack_payload(&no_ldm, input.len()).unwrap(),
            input
        );
        assert_ne!(
            no_ldm,
            compress_compact_frame_with(&input, CompactFrameCompression::DEFAULT).unwrap()
        );
    }

    #[test]
    fn incompressible_input_stays_raw_under_every_policy() {
        // xorshift64 noise: no repeated structure for either encoder to find.
        let mut word = 0x9E37_79B9_7F4A_7C15_u64;
        let input: Vec<u8> = (0..4096)
            .map(|_| {
                word ^= word << 13;
                word ^= word >> 7;
                word ^= word << 17;
                (word >> 56) as u8
            })
            .collect();
        for options in [
            CompactFrameCompression::DEFAULT,
            CompactFrameCompression::ADOPTION,
        ] {
            assert_eq!(compress_compact_frame_with(&input, options).unwrap(), input);
        }
    }
}
