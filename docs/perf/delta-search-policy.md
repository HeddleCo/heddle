# Delta-search policy and measurements

Status: measured 2026-08-02 against Heddle `a37381633b0cacaa987bc2191fa48fd6c36955d9`.

Heddle's classic pack builder can substantially reduce pack size by searching
recent objects for delta bases. Import/adoption uses a streaming builder by
default to bound memory, while snapshot packing and foreground GC prioritize
latency. These paths therefore keep delta search off by default and expose
independent repository settings:

```toml
[storage.delta_search]
import = false
snapshot = false
gc = false
```

`import` applies to full Git import/adoption paths, including Git-backed clone,
pull, and Git Projection import. Enabling it selects the buffered classic pack
builder instead of the streaming builder. `snapshot` applies to packs created
while capturing batches of new objects. `gc` applies to native-object packing
during ordinary `heddle maintenance gc`; `--aggressive` continues to force
delta search on regardless of the setting.

For a repository that has not yet been adopted, run `heddle init`, set
`storage.delta_search.import = true` in `.heddle/config.toml`, and then run
`heddle adopt`.

## Method

The three repositories retained by the semantic-packing spike were measured at
these exact revisions:

| Corpus | Revision |
| --- | --- |
| ripgrep | `435f59fc4b43af3ab32f34d53fa34978f393fe52` |
| fd | `41532d114e2ba565fb5367d606c111b29b96450c` |
| cargo-deny | `bca0dde53651ee946720e4540b5ce2610bec8f06` |

The release binary was built with `cargo build --release -p heddle-cli
--features zstd`. Each import measurement used a fresh local clone and an
already-initialized Heddle repository; `/usr/bin/time -v` wrapped only `heddle
adopt`. Off/on runs were sequential, with order alternated by corpus to reduce
systematic warm-cache bias. Filesystem caches were not dropped. These are
single-run engineering measurements, so the ratios are policy signals rather
than benchmark confidence intervals.

For GC, one small capture was added after delta-off adoption so the store had
multiple packs to consolidate. That prepared repository was copied byte for
byte into the off/on cases before changing only
`storage.delta_search.gc`. `/usr/bin/time -v` then wrapped ordinary,
non-aggressive `heddle maintenance gc`. Pack bytes and entry types were read
directly from the resulting LMPK v3 pack and index files. Pack size below
excludes the index, whose size was identical between each off/on pair.

## Import/adoption results

| Corpus | Objects | Pack off -> on | Saved | Deltas off -> on | Wall off -> on | Slowdown | Peak RSS off -> on |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep | 13,558 | 35.47 -> 11.70 MiB | 67.02% | 0 -> 11,189 | 26.90s -> 253.11s | 9.41x | 228.55 -> 354.36 MiB |
| fd | 8,523 | 17.52 -> 5.79 MiB | 66.93% | 0 -> 6,470 | 24.92s -> 82.36s | 3.30x | 128.91 -> 173.77 MiB |
| cargo-deny | 6,091 | 21.75 -> 10.98 MiB | 49.52% | 0 -> 5,428 | 8.14s -> 126.43s | 15.53x | 156.55 -> 819.97 MiB |

Exact off/on pack byte counts were 37,191,867/12,265,464 for ripgrep,
18,369,339/6,075,152 for fd, and 22,804,383/11,512,222 for cargo-deny.

## Default-GC results

| Corpus | Objects | Pack off -> on | Saved | Deltas off -> on | Wall off -> on | Slowdown | Peak RSS off -> on |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep | 13,734 | 35.60 -> 12.08 MiB | 66.06% | 0 -> 11,364 | 2.71s -> 175.20s | 64.65x | 401.34 -> 366.59 MiB |
| fd | 8,565 | 17.53 -> 5.85 MiB | 66.60% | 0 -> 6,516 | 1.08s -> 47.87s | 44.32x | 202.76 -> 181.70 MiB |
| cargo-deny | 6,271 | 21.87 -> 11.13 MiB | 49.11% | 0 -> 5,601 | 1.46s -> 81.60s | 55.89x | 244.67 -> 782.77 MiB |

Exact off/on pack byte counts were 37,330,288/12,669,574 for ripgrep,
18,376,751/6,137,785 for fd, and 22,927,448/11,667,253 for cargo-deny.

## Byte-exact reconstruction proof

The negative case is named
`delta_search_enabled_import_reconstructs_byte_exact_with_zero_blake3_failures`.
It imports a deliberately delta-friendly Git history with the setting both off
and on, inspects the packs to prove `0` versus `>0` delta entries, reconstructs
every imported blob through `FsStore`, recomputes its typed BLAKE3 content
identity, and requires exactly zero failures.

The retained-corpus harness also loaded every object through `PackReader` and
recomputed typed BLAKE3 identities for every blob. Delta-enabled import rebuilt
28,172 objects and checked 11,397 blobs; delta-enabled GC rebuilt 28,570 objects
and checked 11,789 blobs. Both paths had 0 reconstruction failures and 0 BLAKE3
identity failures on every corpus. `heddle verify --output json` also reported
`clean: true` for all three delta-enabled GC repositories.

## Recommendation

Keep import/adoption default-off. Saving 49.52-67.02% is valuable, but a
3.30-15.53x hot-path slowdown and a cargo-deny peak of 819.97 MiB reject it as
the general import policy. Operators who explicitly prefer stored size can opt
in with `storage.delta_search.import`.

Keep snapshot packing default-off and opt-in. Snapshot capture is latency
sensitive, and it should not inherit the buffered import or whole-store search
cost without a dedicated snapshot benchmark that justifies that change.

Keep foreground default GC delta search off. The measured 44.32-64.65x slowdown
would turn routine maintenance into an unexpectedly long operation. Preserve
`--aggressive` as the explicit one-shot override and expose
`storage.delta_search.gc` for repositories that accept that recurring cost.

The right eventual default home is a background repack/consolidation job with
resource controls and cancellation, where the 49.11-66.60% storage win is
worthwhile and does not block adoption, capture, or foreground maintenance.
Until that scheduler exists, all three synchronous paths remain opt-in.
