# Heddle vs git storage

Single run on 2026-11-04, this machine, this source tree. Not an average.
MiB below means 1,048,576 bytes. Ratios use the exact byte counts.

**After the compact step this CLI actually has (`heddle maintenance repack`), `.heddle` is larger than `.git` on every repository measured, against both `git gc` and `git gc --aggressive`.** The margin against ordinary `git gc` is small on this repository (1.04×) and about 1.2–1.5× on the others. Against `git gc --aggressive` it is 1.2× here and 1.6–4.3× on the others.

The pack files inside that repacked store are smaller than git's aggressive pack on every repository. `metadata.sqlite3` and `ingest/sha_map.sqlite` are what push the total back over `.git`. A fresh `heddle import local`, before repack, is 3.7–14× a `git gc` `.git`.

## Host

| | |
| --- | --- |
| Machine | Linux 6.12.94+, x86_64, 4 CPUs, MemTotal 16,387,280 kB, ext4 |
| git | 2.43.0. `GIT_CONFIG_NOSYSTEM=1`. `GIT_CONFIG_GLOBAL` is an empty file. No system or user git config. |
| heddle | `heddle 0.31.3`, built from `518a04d2e7c9dd319dfd981af9b9163fd1db07fd` with `cargo build --locked --release -p heddle-cli --bin heddle`. Default features, which include `zstd`. Release profile is `lto = true`, `codegen-units = 1`, `strip = true`. |
| Identity on every heddle command | `HEDDLE_PRINCIPAL_NAME=Storage Benchmark`, `HEDDLE_PRINCIPAL_EMAIL=storage-benchmark@example.com` |

`du -sb` (apparent size) is the number in the tables. Allocated `du -s --block-size=1` was within about 2% of apparent size on these trees; the tables do not switch to it.

## What was compared

One commit per repository. The history is every ancestor of that commit. Not a shallow clone (`git rev-parse --is-shallow-repository` was `false`; no `.git/shallow`). Not every branch on the remote. The same mirror was cloned four times with `git clone --no-local --no-tags`, so git gc and heddle import did not share pack files.

`git rev-list --count HEAD` matched `commits_imported`. `sha_map` row counts (commit + tree + blob) matched `git cat-file --batch-all-objects` on every repository.

`import local` writes straight into a pack. After import, loose blobs were 0, loose trees 1, loose states 1. There is no loose-object store to measure. The states below are the ones the CLI produced:

| State | How |
| --- | --- |
| git `gc` | `git gc` on a fresh clone. On every repo this left `size-pack` unchanged and added a few MB of non-pack files. That is the ordinary-user number. |
| git `gc --aggressive` | Separate clone. This did recompress. |
| import | `heddle import local --ref measured` |
| gc | `heddle maintenance gc` on a copy of that import |
| gc --aggressive | `heddle maintenance gc --aggressive` on a fresh copy of the import, not after the default gc. Default gc leaves one pack, and `--aggressive` then reports `packed_count: 0`. |
| repack | `heddle maintenance repack` on a fresh copy of the import |
| delta import | Separate clone: `heddle init`, set `storage.delta_search.import = true`, then `heddle import local --ref measured` |
| delta + repack | `heddle maintenance repack` on that delta import |

The delta rows are not a smaller copy of the one-step import. Repack pack bytes match across the two paths (see below). The delta path never wrote `metadata.sqlite3`.

An adopted checkout still has `.git`. Import did not shrink it (it matched the pre-gc clone). The table compares the two stores. Disk for an adopted tree is approximately both.

## Repositories

All five imports exited 0. Curl and git/git reported `verification_status: uncaptured`. The other three reported `clean`. Object counts still matched, as above.

| Name | Why | Commit | Commits | Merges | Revert subjects | Git objects (commit/tree/blob) |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| heddle | This repository. Rust, moderate history, a few large JSON fixtures at the tip. | `518a04d2e7c9dd319dfd981af9b9163fd1db07fd` | 991 | 7 | 3 | 28,884 (991 / 12,593 / 15,300) |
| curl | Long C history of similar `.c`/`.h` files. Tip is mostly C. | `41e2abc56c1fc2abcf76b6e5e77c2e3de4051888` | 40,344 | 24 | 160 | 297,317 (40,344 / 108,274 / 148,699) |
| babel | JS/TS monorepo. Tip: 15,227 `.js`, 1,718 `.ts`, 9,889 `.json`. Many trees (224,735). | `404f0a2d8405782440e5dc4b44c40df59448a136` | 18,767 | 1,396 | 130 | 394,840 (18,767 / 224,735 / 151,338) |
| pdf.js | Binary-heavy. Tip blobs: 156,966,966 bytes of `.pdf` (1,079 files) against 8,146,036 bytes of `.js`. 10,111 merge commits. | `d2d5bea26f6ce9d647cc09cce1f61d47a2244f7e` | 24,392 | 10,111 | 124 | 134,859 (24,392 / 64,352 / 46,115) |
| git | Long history, many merges and reverts, many renames in the project's own history. | `750f5de7dfe9a4062f95088212b063f189d50e91` | 82,676 | 21,402 | 237 | 410,303 (82,676 / 164,668 / 162,959) |

Uncompressed git payload (sum of `cat-file` object sizes), blobs / trees:

| | Blobs | Trees |
| --- | ---: | ---: |
| heddle | 701,572,703 | 8,808,238 |
| curl | 3,437,342,083 | 459,921,235 |
| babel | 1,707,207,790 | 133,782,693 |
| pdf.js | 2,337,501,957 | 114,190,486 |
| git | 5,608,367,862 | 2,574,702,153 |

## Summary

Bytes, then MiB.

| Repo | git gc | git aggressive | import | heddle gc | heddle gc --aggressive | repack | delta import | delta + repack |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| heddle | 51,938,991 | 45,030,214 | 191,308,911 | 190,429,015 | 118,013,846 | 53,948,192 | 110,000,723 | 46,849,787 |
| curl | 167,131,552 | 67,403,608 | 1,715,141,420 | 1,714,767,282 | 817,765,813 | 258,750,270 | 672,166,993 | 136,786,084 |
| babel | 167,395,957 | 89,587,764 | 772,974,292 | 771,179,532 | 429,006,759 | 243,631,816 | 360,350,405 | 188,880,376 |
| pdf.js | 238,077,915 | 174,145,692 | 916,412,866 | 915,082,672 | 495,959,020 | 280,010,077 | 405,291,423 | 198,290,006 |
| git | 334,140,501 | 116,908,685 | 4,677,341,949 | 4,675,272,931 | 1,518,627,121 | 502,311,011 | 1,197,307,419 | 215,586,261 |

| Repo | git gc | git aggressive | import | heddle gc | heddle gc --aggressive | repack | delta import | delta + repack |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| heddle | 49.53 | 42.94 | 182.45 | 181.61 | 112.55 | 51.45 | 104.90 | 44.68 |
| curl | 159.39 | 64.28 | 1635.69 | 1635.33 | 779.88 | 246.76 | 641.03 | 130.45 |
| babel | 159.64 | 85.44 | 737.17 | 735.45 | 409.13 | 232.35 | 343.66 | 180.13 |
| pdf.js | 227.05 | 166.08 | 873.96 | 872.69 | 472.98 | 267.04 | 386.52 | 189.10 |
| git | 318.66 | 111.49 | 4460.66 | 4458.69 | 1448.28 | 479.04 | 1141.84 | 205.60 |

### Ratios for the one-step import, then repack

This is the compact state a normal `import local` can reach. Heddle/git and git/Heddle.

| Repo | repack / git gc | git gc / repack | repack / git aggressive | git aggressive / repack |
| --- | ---: | ---: | ---: | ---: |
| heddle | 1.039 | 0.963 | 1.198 | 0.835 |
| curl | 1.548 | 0.646 | 3.839 | 0.260 |
| babel | 1.455 | 0.687 | 2.719 | 0.368 |
| pdf.js | 1.176 | 0.850 | 1.608 | 0.622 |
| git | 1.503 | 0.665 | 4.297 | 0.233 |

Import with no repack, against ordinary `git gc`: heddle 3.683×, curl 10.262×, babel 4.618×, pdf.js 3.849×, git 13.998×.

`maintenance gc` changes the import by under 1% on every repo. It rewrites the pack. `packed_count` was the git object count plus one (28,885 here, against 28,884 git objects; the same +1 on the other four). `bytes_saved` in the CLI's JSON is not a `du` delta; on this repo it printed 543,989,473 while `du` fell by 879,896. Every repo reported `pruned_loose: 1` and `bytes_freed: 5`.

`gc --aggressive` does shrink the import, and it is still larger than git's aggressive `.git` by 2.621× (heddle), 12.132× (curl), 4.789× (babel), 2.848× (pdf.js), and 12.990× (git).

### Delta import is a different tree

`[storage.delta_search] import = true` is off by default. Turning it on, the way `docs/perf/delta-search-policy.md` describes, produced a smaller import pack. After `maintenance repack`, the pack directory bytes match the one-step repack within a few kilobytes:

| Repo | repack `packs/` | delta + repack `packs/` |
| --- | ---: | ---: |
| heddle | 38,682,893 | 38,683,231 |
| curl | 53,125,125 | 53,123,813 |
| babel | 70,082,322 | 70,082,495 |
| pdf.js | 159,186,713 | 159,186,307 |
| git | 96,047,353 | 96,047,065 |

The smaller delta+repack *total* is `metadata.sqlite3`, which the one-step import wrote and the init-then-import path did not. Those files were 7,077,888 (heddle), 121,843,712 (curl), 54,792,192 (babel), 81,764,352 (pdf.js), and 286,720,000 (git). They account for the gap between the two totals; the pack directories do not. Do not read the delta+repack column as the same store with a better encoding.

## Wall time (seconds)

One sample each. Peak RSS is the highest VmHWM seen in the process tree, sampled about every 50 ms.

| Repo | git gc | git aggressive | import | heddle gc | heddle aggressive | repack | delta import | delta repack |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| heddle | 0.4 | 14.8 | 31.1 | 3.4 | 968.0 | 54.0 | 1,014.2 | 67.0 |
| curl | 2.4 | 58.2 | 1,121.3 | 34.9 | 4,221.4 | 1,698.7 | 5,234.3 | 1,819.0 |
| babel | 2.6 | 51.2 | 519.5 | 18.9 | 1,334.4 | 833.5 | 1,539.3 | 915.1 |
| pdf.js | 1.8 | 65.6 | 687.6 | 20.9 | 4,620.3 | 1,022.2 | 5,032.8 | 1,124.0 |
| git | 6.3 | 116.7 | 2,631.6 | 106.1 | 2,576.9 | 4,114.8 | 5,993.4 | 4,473.3 |

The slow Heddle commands are the ones that delta-search. Peak RSS for those was about 1.0 GB (this repo), 5.6 GB (curl aggressive gc), 3.7 GB (pdf.js aggressive gc), 13.4 GB (git aggressive gc), and 10.7 GB (git delta import). Git's own `gc --aggressive` stayed under 1.7 GB and finished in under two minutes even on git's history.

## Where the bytes are after repack

Repack is the smallest state the one-step import reached. Pack headers measured on disk: `4c4d504b` (`LMPK`) for `.pack`, `4c4d4900` for `.idx`, `4e504b31` (`NPK1`) for `.npk`.

| Repo | `.heddle` | `.pack` | `.npk` | `.idx` | `sha_map.sqlite` | `metadata.sqlite3` | `state/` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| heddle | 53,948,192 | 36,272,447 | 1,758,750 | 651,696 | 7,512,064 | 7,077,888 | 607,324 |
| curl | 258,750,270 | 28,261,487 | 17,301,862 | 7,561,776 | 82,808,832 | 121,843,712 | 904,578 |
| babel | 243,631,816 | 37,420,957 | 25,857,109 | 6,804,256 | 104,071,168 | 54,792,192 | 14,618,108 |
| pdf.js | 280,010,077 | 148,997,435 | 7,368,942 | 2,820,336 | 38,359,040 | 81,764,352 | 631,958 |
| git | 502,311,011 | 61,635,971 | 24,582,006 | 9,829,376 | 118,394,880 | 286,720,000 | 1,080,767 |

Those three pack files together, against git's aggressive `objects/pack`:

| Repo | Heddle pack files | git aggressive `objects/pack` |
| --- | ---: | ---: |
| heddle | 38,682,893 | 44,692,660 |
| curl | 53,125,125 | 64,540,436 |
| babel | 70,082,322 | 83,126,520 |
| pdf.js | 159,186,713 | 172,388,237 |
| git | 96,047,353 | 111,456,221 |

The pack files are the smaller side of that comparison. The two SQLite files are the larger side of `.heddle` once the history is big: on curl they are 204,652,544 bytes against a 67,403,608-byte aggressive `.git`; on git they are 405,114,880 bytes against a 116,908,685-byte aggressive `.git`. `sha_map` is one row per git object (the counts above). `metadata.sqlite3` is the one-step import's thread and operation database; it is absent from the delta-import tree.

Before repack, the import `.pack` is the dominant file: 174,953,626 of 191,308,911 here; 1,497,688,969 of 1,715,141,420 on curl; 4,254,731,545 of 4,677,341,949 on git. Loose object directories are a few hundred bytes.

pdf.js is the shape where git's aggressive gc helps least (238,077,915 → 174,145,692, and `size-pack` 230,254 → a still-large pack). Heddle's import is 3.8× that ordinary `.git`, not 10×, and the repacked `.pack` is almost entirely the PDF payload (148,997,435 of the 159 MB of pack files). Curl and git are the shapes where git's deltas pay: aggressive `.git` is 67 MB and 117 MB, and Heddle's non-delta import pack stays near 1.5 GB and 4.3 GB until repack, which still leaves a store 3.8× and 4.3× git's aggressive result.

## Commands

Script: `scripts/measure-heddle-vs-git-storage.py`. Per repository it does the following. `<sha>` is the commit in the table. The work directory must not already contain that repository's clones.

```bash
cargo build --locked --release -p heddle-cli --bin heddle

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL="$WORK/empty-gitconfig"   # empty file
export HEDDLE_PRINCIPAL_NAME="Storage Benchmark"
export HEDDLE_PRINCIPAL_EMAIL="storage-benchmark@example.com"

git init --bare --quiet "$MIRROR"
git --git-dir "$MIRROR" fetch --no-tags --no-recurse-submodules \
  "$URL" "<sha>:refs/heads/measured"
git --git-dir "$MIRROR" symbolic-ref HEAD refs/heads/measured

git clone --quiet --no-local --no-tags "$MIRROR" "$GIT_DEFAULT"
git -C "$GIT_DEFAULT" gc
git clone --quiet --no-local --no-tags "$MIRROR" "$GIT_AGGRESSIVE"
git -C "$GIT_AGGRESSIVE" gc --aggressive

git clone --quiet --no-local --no-tags "$MIRROR" "$HEDDLE"
heddle --output json import local --ref measured    # cwd: $HEDDLE
# copies of $HEDDLE, then one of:
heddle --output json maintenance gc
heddle --output json maintenance gc --aggressive
heddle --output json maintenance repack

git clone --quiet --no-local --no-tags "$MIRROR" "$DELTA"
heddle --output json init                         # cwd: $DELTA
# in .heddle/config.toml, existing section:
#   [storage.delta_search]
#   import = true
heddle --output json import local --ref measured
heddle --output json maintenance repack
```

The invocation that produced this document:

```bash
python3 scripts/measure-heddle-vs-git-storage.py \
  --work-dir /var/tmp/heddle-vs-git-storage \
  --heddle target/release/heddle \
  --source-root . \
  --repo heddle=https://github.com/HeddleCo/heddle.git@518a04d2e7c9dd319dfd981af9b9163fd1db07fd \
  --repo curl=https://github.com/curl/curl.git@41e2abc56c1fc2abcf76b6e5e77c2e3de4051888 \
  --repo babel=https://github.com/babel/babel.git@404f0a2d8405782440e5dc4b44c40df59448a136 \
  --repo pdfjs=https://github.com/mozilla/pdf.js.git@d2d5bea26f6ce9d647cc09cce1f61d47a2244f7e \
  --repo git=https://github.com/git/git.git@750f5de7dfe9a4062f95088212b063f189d50e91
```

## Conclusion

Heddle's store, as `import local` leaves it and as `maintenance repack` compacts it, is larger than git's on every history measured here. Ordinary `git gc` does not recompress a fresh GitHub pack; the honest everyday comparison is still a loss, by 4% on this repository and by 18–55% on the other four. The best git pack (`gc --aggressive`) is smaller still, by 20% here and by 1.6–4.3× elsewhere. The bytes that keep `.heddle` ahead of its own pack files are `metadata.sqlite3` and `ingest/sha_map.sqlite`. The bytes that keep the unrepacked import far ahead of git, on text history, are the non-delta `.pack`.
