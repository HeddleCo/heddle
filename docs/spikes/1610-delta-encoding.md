# Heddle #1610: content-delta encoding spike

## Recommendation

Do **not** replace HCB2's full bodies plus solid zstd with chained content deltas on the strength of this spike.

Explicit deltas did not produce a material compression win. Across the synthetic revision chains, delta+zstd ranged from 0.76% smaller to 0.48% larger than HCB2+zstd. On the real 32-version `Cargo.lock` lineage it was 6.87% larger (55,958 versus 52,360 bytes). Independent blobs were 0.71-0.78% larger with delta metadata. Zstd LDM is already finding almost all of the inter-revision redundancy.

Delta encoding also lost the write path. Delta+zstd was 4.8-31.5x slower to encode the similar synthetic chains and 1.60x slower on the real lineage. The plain delta residual usually had essentially the same size as solid HCB2, but was still 5-13x slower to produce on similar synthetic data. The Git-style differ used here favors a small implementation over the most optimized possible differ, so the absolute encode gap is implementation-specific; the absence of a compression gap is not.

Content delta did win full-decode and deepest-revision read latency on similar lineages. Despite walking 128 predecessor deltas, it found the newest synthetic blob in 5.1-6.9 ms versus 38.6-41.6 ms for HCB2+zstd. On real `Cargo.lock`, the content-ID read was 1.16 ms versus 2.32 ms. The reason is not seekability: the delta residual is tiny and its copy instructions are cheap to apply, while solid zstd inflates the whole full-body frame. On independent blobs, delta lost this advantage and at 128 revisions delta+zstd was 5.45x slower to fully decode and 4.53x slower for the content-ID read.

An ordinal index alone does not fix either compressed format. It lets raw HCB2 jump directly to a full body, but solid HCB2 still must inflate the frame and a chained delta still must resolve from a base or checkpoint. Seekable/chunked zstd could recover HCB2 random access; periodic full checkpoints could bound delta depth, at a compression cost. Those are separate format experiments and are not justified as production changes here.

The data supports keeping the current format. If read latency is the actual problem, the next falsifier should measure smaller/seekable solid frames or cached decompressed frames. If write CPU on dissimilar data is the problem, similarity classification or an early incompressibility escape is more promising: level-19 HCB2 spent 6.93 s proving a 32.25 MiB random lineage should stay raw, while raw HCB2 encoded it in 18.9 ms. This spike does not choose either design.

## What was measured

The harness is `crates/objects/examples/delta_encoding_spike.rs`. It changes no production encoder, decoder, or pack-format path. Run it with:

```text
HEDDLE_DELTA_SPIKE_SAMPLES=3 cargo run -p heddle-objects \
  --example delta_encoding_spike --features bench,zstd --release
```

The four measured encodings are:

- **CR — current raw:** the production `encode_blob_frame`, without zstd.
- **CZ — current zstd:** `encode_blob_frame` followed by the production `compress_compact_frame`: zstd level 19, `window_log=27`, long-distance matching, checksum, and the production fallback to raw when compression is not smaller.
- **DR — delta raw:** a prototype checksummed frame containing one full base and a delta from each immediately previous revision to the next. The differ is Heddle's existing `heddle_format::delta::DeltaEncoder`: a Git-style copy/insert stream backed by a 4-byte base index, a 16-byte minimum match for these blobs, and at most 1,024 candidates per key.
- **DZ — delta zstd:** the same delta frame passed through the exact production `compress_compact_frame` policy.

The prototype frame adds a four-byte discriminator, object count, target and section lengths, and a BLAKE3 checksum. Content IDs remain external in the same sense as the pack index; this is measurement framing, not a proposed wire format.

Corpora:

- Every synthetic corpus begins with a deterministic, incompressible 256 KiB base and has 8, 32, or 128 subsequent revisions (9, 33, or 129 objects total).
- `local64` replaces one localized 64-byte range per revision.
- `scatter8x8` replaces eight independently located 8-byte ranges per revision.
- `local4096` replaces one localized 4 KiB range per revision.
- `independent-random` generates every 256 KiB object independently; this is the negative control.
- The real corpus is 32 chronological `Cargo.lock` versions from commits `d6004db093f4e4d8e91e397c323912c967eca70b` through `813462391674741c76d0af8ea1409d1c4f8a3e39`. It contains 6,482,374 raw bytes; individual versions range from 195,619 to 206,752 bytes.

The per-object baseline is the sum of compressing each body independently with the same production zstd policy. The synthetic bodies are deliberately individually incompressible, so that baseline equals raw size; the repeated-version relationship is the compressible signal. The real per-object baseline is 1,297,948 bytes.

## Method and caveats

- Machine: AMD Ryzen 7 7700, Rust 1.98.0 (`88d9e12ae`, 2026-08-18), release build, Git HEAD `813462391674741c76d0af8ea1409d1c4f8a3e39`.
- Encode and full-decode times are medians of three complete operations. Random reads are medians of nine operations. Times include frame verification; content-ID lookup itself is outside the timed region.
- The random target is always the newest/deepest revision. `ID read` emulates the current lookup semantics: HCB2 uses `decode_blob_frame` and hashes all bodies to find the requested ID; delta walks the chain and hashes each reconstructed body until it finds the ID.
- `Indexed read` assumes an additional content-ID-to-ordinal index. HCB2 verifies the frame, parses lengths, and copies only the selected body; delta verifies the frame and resolves only through that ordinal. It is a best-case format capability measurement, not today's production API.
- RSS is sampled from `/proc/self/statm` every 0.5 ms after `malloc_trim(0)`. Each memory cell is `process peak (+increase over the prepared-input baseline)` in MiB. The increase is the better cross-format comparison; the absolute read peak also includes the source corpus still held by the harness. Very short operations may be represented by the final resident-set sample rather than an interior peak.
- Results are one machine and one real lineage. Three samples are enough to expose order-of-magnitude effects, not to establish a tight performance contract. The differ is real and round-trip checked, but it is not a tuned production delta packer.

## Compression data

Each encoding cell is `bytes [stored mode; ratio to raw; ratio to per-object zstd]`, where `r` means the production compressor kept the frame raw and `z` means it retained zstd output.

| Corpus | Objects | Raw | Per-object zstd | CR | CZ | DR | DZ |
|---|---:|---:|---:|---:|---:|---:|---:|
| synthetic-local64-r8 | 9 | 2359296 | 2359296 | 2359344 [r; 1.000020; 1.000020] | 262953 [z; 0.111454; 0.111454] | 262928 [r; 0.111443; 0.111443] | 262928 [r; 0.111443; 0.111443] |
| synthetic-scatter8x8-r8 | 9 | 2359296 | 2359296 | 2359344 [r; 1.000020; 1.000020] | 263097 [z; 0.111515; 0.111515] | 263312 [r; 0.111606; 0.111606] | 263312 [r; 0.111606; 0.111606] |
| synthetic-local4096-r8 | 9 | 2359296 | 2359296 | 2359344 [r; 1.000020; 1.000020] | 295308 [z; 0.125168; 0.125168] | 295440 [r; 0.125224; 0.125224] | 295440 [r; 0.125224; 0.125224] |
| independent-random-r8 | 9 | 2359296 | 2359296 | 2359344 [r; 1.000020; 1.000020] | 2359344 [r; 1.000020; 1.000020] | 2376000 [r; 1.007080; 1.007080] | 2376000 [r; 1.007080; 1.007080] |
| synthetic-local64-r32 | 33 | 8650752 | 8650752 | 8650824 [r; 1.000008; 1.000008] | 265162 [z; 0.030652; 0.030652] | 265152 [r; 0.030651; 0.030651] | 265152 [r; 0.030651; 0.030651] |
| synthetic-scatter8x8-r32 | 33 | 8650752 | 8650752 | 8650824 [r; 1.000008; 1.000008] | 265708 [z; 0.030715; 0.030715] | 266641 [r; 0.030823; 0.030823] | 266536 [z; 0.030811; 0.030811] |
| synthetic-local4096-r32 | 33 | 8650752 | 8650752 | 8650824 [r; 1.000008; 1.000008] | 394588 [z; 0.045613; 0.045613] | 395200 [r; 0.045684; 0.045684] | 395200 [r; 0.045684; 0.045684] |
| independent-random-r32 | 33 | 8650752 | 8650752 | 8650824 [r; 1.000008; 1.000008] | 8650824 [r; 1.000008; 1.000008] | 8717400 [r; 1.007704; 1.007704] | 8717400 [r; 1.007704; 1.007704] |
| synthetic-local64-r128 | 129 | 33816576 | 33816576 | 33816745 [r; 1.000005; 1.000005] | 274000 [z; 0.008103; 0.008103] | 274008 [r; 0.008103; 0.008103] | 271930 [z; 0.008041; 0.008041] |
| synthetic-scatter8x8-r128 | 129 | 33816576 | 33816576 | 33816745 [r; 1.000005; 1.000005] | 276181 [z; 0.008167; 0.008167] | 279955 [r; 0.008279; 0.008279] | 277512 [z; 0.008206; 0.008206] |
| synthetic-local4096-r128 | 129 | 33816576 | 33816576 | 33816745 [r; 1.000005; 1.000005] | 792273 [z; 0.023429; 0.023429] | 794200 [r; 0.023486; 0.023486] | 792289 [z; 0.023429; 0.023429] |
| independent-random-r128 | 129 | 33816576 | 33816576 | 33816745 [r; 1.000005; 1.000005] | 33816745 [r; 1.000005; 1.000005] | 34083000 [r; 1.007879; 1.007879] | 34081834 [z; 1.007844; 1.007844] |
| real-Cargo.lock-v32 | 32 | 6482374 | 1297948 | 6482458 [r; 1.000013; 4.994390] | 52360 [z; 0.008077; 0.040341] | 219982 [r; 0.033935; 0.169484] | 55958 [z; 0.008632; 0.043113] |

## Speed data

Each cell is `encode / full decode / ID read / indexed read`, in milliseconds.

| Corpus | CR | CZ | DR | DZ |
|---|---:|---:|---:|---:|
| synthetic-local64-r8 | 0.292 / 0.539 / 0.558 / 0.262 | 44.518 / 0.729 / 0.760 / 0.472 | 214.222 / 0.076 / 0.352 / 0.071 | 281.547 / 0.077 / 0.356 / 0.073 |
| synthetic-scatter8x8-r8 | 0.478 / 0.532 / 0.555 / 0.264 | 50.959 / 0.734 / 0.752 / 0.474 | 244.601 / 0.107 / 0.385 / 0.106 | 245.747 / 0.114 / 0.395 / 0.106 |
| synthetic-local4096-r8 | 0.340 / 0.536 / 0.557 / 0.262 | 42.876 / 0.731 / 0.763 / 0.487 | 251.909 / 0.080 / 0.354 / 0.078 | 238.841 / 0.085 / 0.361 / 0.078 |
| independent-random-r8 | 0.467 / 0.550 / 0.555 / 0.263 | 175.411 / 0.533 / 0.558 / 0.262 | 277.655 / 0.370 / 0.656 / 0.377 | 415.128 / 0.370 / 0.671 / 0.379 |
| synthetic-local64-r32 | 1.229 / 2.031 / 2.037 / 1.155 | 126.000 / 9.498 / 9.741 / 8.275 | 874.187 / 0.261 / 1.317 / 0.250 | 903.402 / 0.262 / 1.317 / 0.254 |
| synthetic-scatter8x8-r32 | 1.597 / 2.459 / 2.032 / 0.961 | 114.832 / 9.433 / 9.424 / 8.369 | 889.717 / 0.335 / 1.397 / 0.330 | 955.730 / 0.357 / 1.422 / 0.357 |
| synthetic-local4096-r32 | 1.440 / 2.025 / 2.035 / 0.956 | 136.358 / 9.344 / 9.412 / 8.569 | 887.510 / 0.277 / 1.340 / 0.270 | 915.634 / 0.271 / 1.336 / 0.271 |
| independent-random-r32 | 1.240 / 2.023 / 2.033 / 0.954 | 1071.619 / 2.033 / 2.029 / 0.956 | 1013.226 / 1.475 / 3.045 / 1.730 | 2111.703 / 1.557 / 2.494 / 1.420 |
| synthetic-local64-r128 | 17.193 / 9.677 / 10.201 / 4.559 | 376.494 / 39.502 / 38.562 / 33.435 | 3773.207 / 2.522 / 6.249 / 0.883 | 3685.771 / 11.050 / 5.117 / 0.904 |
| synthetic-scatter8x8-r128 | 18.498 / 9.391 / 9.512 / 4.495 | 353.768 / 39.243 / 39.052 / 33.608 | 4450.883 / 12.350 / 6.573 / 1.202 | 11133.257 / 11.778 / 5.507 / 1.212 |
| synthetic-local4096-r128 | 17.321 / 9.551 / 9.381 / 4.479 | 425.024 / 38.589 / 41.633 / 34.470 | 4802.659 / 12.006 / 5.185 / 0.955 | 3731.880 / 11.605 / 6.925 / 1.111 |
| independent-random-r128 | 18.884 / 11.922 / 12.165 / 5.428 | 6933.579 / 10.000 / 9.708 / 4.676 | 4042.898 / 17.646 / 11.512 / 7.233 | 10516.026 / 54.476 / 43.979 / 39.779 |
| real-Cargo.lock-v32 | 1.166 / 1.579 / 1.583 / 0.715 | 137.754 / 2.337 / 2.321 / 1.419 | 171.003 / 0.249 / 1.325 / 0.265 | 220.510 / 0.319 / 1.162 / 0.303 |

## Memory data

Each cell is `encode peak (+increment) / ID-read peak (+increment)`, in MiB.

| Corpus | CR | CZ | DR | DZ |
|---|---:|---:|---:|---:|
| synthetic-local64-r8 | 11.79 (+2.27) / 9.89 (+0.25) | 63.74 (+57.47) / 11.16 (+4.89) | 50.74 (+44.32) / 7.20 (+0.77) | 50.50 (+44.29) / 7.02 (+0.82) |
| synthetic-scatter8x8-r8 | 13.90 (+4.00) / 10.15 (+0.25) | 63.70 (+57.31) / 11.28 (+4.89) | 50.70 (+44.25) / 9.32 (+2.88) | 50.21 (+44.04) / 9.00 (+2.81) |
| synthetic-local4096-r8 | 13.16 (+3.26) / 10.14 (+0.25) | 63.58 (+57.19) / 11.28 (+4.90) | 50.21 (+43.81) / 7.31 (+0.90) | 50.46 (+44.28) / 7.09 (+0.89) |
| independent-random-r8 | 13.64 (+3.76) / 10.12 (+0.25) | 67.32 (+59.20) / 8.36 (+0.25) | 58.21 (+48.28) / 11.16 (+1.23) | 109.54 (+101.38) / 9.42 (+1.24) |
| synthetic-local64-r32 | 32.18 (+12.02) / 20.40 (+0.25) | 121.21 (+108.82) / 33.03 (+20.65) | 56.21 (+43.76) / 16.89 (+4.45) | 56.21 (+44.00) / 15.98 (+3.77) |
| synthetic-scatter8x8-r32 | 29.69 (+9.51) / 20.43 (+0.25) | 119.74 (+107.33) / 33.32 (+20.91) | 56.33 (+43.92) / 17.70 (+5.29) | 56.61 (+44.20) / 17.62 (+5.22) |
| synthetic-local4096-r32 | 30.12 (+10.01) / 20.36 (+0.25) | 119.68 (+107.32) / 33.02 (+20.66) | 56.55 (+44.16) / 16.53 (+4.14) | 56.27 (+44.01) / 16.29 (+4.03) |
| independent-random-r32 | 31.88 (+11.76) / 20.36 (+0.25) | 137.43 (+117.32) / 20.35 (+0.25) | 88.29 (+60.32) / 29.34 (+1.35) | 178.01 (+157.80) / 21.45 (+1.24) |
| synthetic-local64-r128 | 116.62 (+48.51) / 68.36 (+0.25) | 230.17 (+193.83) / 116.64 (+80.30) | 80.26 (+43.85) / 41.81 (+5.41) | 80.34 (+43.93) / 42.06 (+5.65) |
| synthetic-scatter8x8-r128 | 124.39 (+56.26) / 68.37 (+0.25) | 234.20 (+197.83) / 120.24 (+83.88) | 80.63 (+44.16) / 45.26 (+8.79) | 80.62 (+44.16) / 43.55 (+7.09) |
| synthetic-local4096-r128 | 118.68 (+50.52) / 68.42 (+0.26) | 231.74 (+194.83) / 117.20 (+80.29) | 81.16 (+44.20) / 43.21 (+6.25) | 81.57 (+44.62) / 45.01 (+8.05) |
| independent-random-r128 | 120.45 (+52.27) / 68.43 (+0.25) | 298.13 (+229.96) / 68.42 (+0.25) | 179.48 (+111.06) / 69.65 (+1.23) | 314.52 (+246.11) / 152.70 (+84.28) |
| real-Cargo.lock-v32 | 40.27 (+17.01) / 23.45 (+0.19) | 119.45 (+108.09) / 28.02 (+16.66) | 19.48 (+7.41) / 14.01 (+1.92) | 24.23 (+12.45) / 13.75 (+1.96) |

## What surprised me

1. The real lineage favored solid HCB2 by more than the synthetic data did. Delta+zstd was 6.87% larger, while HCB2 solid compression reduced the independently compressed baseline by another 95.97%. Zstd benefits from repetition within each lockfile and across complete lockfiles; turning the lineage into copy instructions removes some of that structure.
2. Chained deltas beat solid-zstd random reads on similar data even at depth 128. The chain itself was not the dominant cost at this blob size; inflating and hashing 32.25 MiB of full bodies was.
3. Zstd on residuals was often useless and occasionally pathological. It was rejected as larger for every 8-revision synthetic delta, localized/scattered cases at depth 32 except `scatter8x8`, and random depth 8/32. For `scatter8x8-r128`, it took delta encode from 4.45 s to 11.13 s to save 2,443 bytes.
4. Memory and time did not move together. On similar 129-object lineages, delta encode used about +44 MiB peak RSS versus +194-198 MiB for zstd LDM, but was roughly an order of magnitude slower. On random data, stacking delta and zstd was worst on both axes.

## Surfaces and scope

- **The verb:** not applicable; no CLI surface changed.
- **Human and agent output:** not applicable; the harness emits CSV for the report.
- **Git interop:** the real corpus is read from Git history, but no import/export path changed.
- **The wire:** not applicable; the `D161` wrapper is benchmark-only and is not a format proposal.
- **Reverse states:** not applicable; no persistent state is written.
