# Apex Package 0: fork-only Biscuit measurement

Measurement spike for [#1808](https://github.com/HeddleCo/heddle/issues/1808), Package 0 of the Apex plan, which gates decision D-01. The raw data is [`p0-baseline.json`](p0-baseline.json) (schema: [`scripts/apex/p0-report.schema.json`](../../../scripts/apex/p0-report.schema.json)), produced by [`scripts/apex/p0-measure.sh`](../../../scripts/apex/p0-measure.sh). The Weft side is `scripts/apex-p0-measure.sh` in the Weft repository.

**This report records numbers only. No Apex code was written, and nothing here recommends a D-01 outcome.**

## What was compared

| Config | Meaning |
|---|---|
| **current** | Each repository at HEAD: crates.io `biscuit-auth 6.0.0` with the features the repository requests today (default set `regex-full`, `datalog-macro`, `pem`, plus `wasm` on wasm32). |
| **fork** (Heddle) | `biscuit-auth` patched to [eclipse-biscuit/biscuit-rust@a6b72596](https://github.com/eclipse-biscuit/biscuit-rust/commit/a6b72596ebe5f391b60e9b91c74edca8febdda93), the merge of PR 306 (the `ToAnyParam` feature-gating fix). Every Heddle manifest requests `default-features = false`, and no feature except `wasm` on wasm32. |
| **fork-full** (Weft) | The same pin with Weft's features minimal. It also path-patches the three *published* Heddle crates in Weft's graph that depend on `biscuit-auth` (`heddle-biscuit-verifier 0.24.1`, `heddle-repo 0.24.1`, `heddleco-capability-verifier 0.20.0`) with their published sources, changed only so that their `biscuit-auth` default features are off. This approximates Weft after a Heddle release that follows #1792. `datalog-macro` stays on as a `weft-hosted` **dev**-dependency for the one real `fact!` test. |
| **fork-pin** (Weft, graph only) | The pin plus Weft's own minimal features, **without** replacing the published Heddle crates. |

**The pin is measurement-only.** It is applied to scratch `git archive HEAD` copies, never to a checkout. `deny.toml` forbids git sources, and the owner decision is to wait for an upstream `biscuit-auth` release (#1792). For inspection only, the exact configurations are on branch `spike/1808-apex-p0-biscuit-pin` in Heddle (9806bcde) and Weft (13ff70cc9). Neither branch has a PR.

Tapestry is measured as-is. It ships a native TypeScript Biscuit encoder and bundles no Rust Biscuit, so the pin cannot change its bundle.

## Environment and method

- **Hardware and OS:** AMD Ryzen 7 7700 (8 cores / 16 threads), 62 GiB RAM, Ubuntu 26.04.1 LTS, kernel 7.0.0-30, x86_64.
- **Toolchain:** rustc 1.98.0 (88d9e12ae 2026-08-18), cargo 1.98.0, node v26.7.0, bun 1.3.14, python 3.14.4, wasm-bindgen 0.2.127.
- **Sources:**
  - heddle `c6395f33` (this branch, which is `3c29acab` main plus the script);
  - weft `48fec2b34` (`aaa3270b7` main plus its script);
  - tapestry `eaa996eb` (main).
  - All three trees were clean.
- **Timing:** every timing is 1 warm-up plus 10 measured runs, alternating current and fork each round (so drift in background load hits both), with an isolated `CARGO_TARGET_DIR` per config. Clean builds delete their target dir before each run. Each sample records wall time, CPU time (user+sys of the whole child process tree) and the 1-minute load average.
- **Latency:** separate processes, alternating configs. Each run times 200 individual operations (50 for the three large-token cases) after an in-process warm-up. The bench reproduces Weft `mint_at`'s authority facts and the offline agent delegation (`AgentAttenuation` plus the signed `pop_delegation` via `key_delegation::append`), and verifies through `heddle_biscuit_verifier::verify_at_with_resource`. Its source is embedded in the script.
- **Sizes:** built once per config (sizes are deterministic for a fixed toolchain and lockfile), using each repository's shipping command:
  - Heddle `release.yml`: `--release -p heddle-cli --features mount,client`;
  - Weft `railpack.json`: `--profile production -p weft-server --features postgres,s3,semantic,embeddings`.

### Load and variance caveats

This is a shared development box, and other agents were building throughout.

- **Load during samples:**
  - builds: 1-minute load 6.7–36.7 on 16 threads;
  - latency: 4.6–6.3.
- **Build wall time is noisy.** Its coefficient of variation was 5–39%. CPU time is much steadier (CV 1–10%), so the comparisons below lean on CPU p50.
- **Small deltas are not resolved.** A difference smaller than about one CPU standard deviation (reported per row) should be read as "no measurable difference".
- **Latency runs were quiet.** The run-to-run spread of medians is shown per row, and it is within a few percent.
- **Sizes and graphs are unaffected by load.**
- **Scale:** the whole-application *clean* build was not timed at N=10. The release builds used for sizes took one sample each (recorded in the JSON as informational). The timed clean builds cover `biscuit-auth` with its full dependency closure, the Heddle verifier crates, and `weft-authz`.
- **Reruns:** an earlier attempt that day was stopped by the script's disk guard when free space fell below the stop line (unrelated activity on the box). All numbers here come from the single complete `--all` run that followed.

## Results

### Dependency graph

| Graph | Config | Packages | Duplicate names | Crypto/protobuf duplicates | biscuit-auth features | proc-macro-error2 | Only reachable via biscuit-auth |
|---|---|---:|---:|---:|---|---|---:|
| heddle workspace-all-targets | current | 704 | 72 | 35 | biscuit-quote, datalog-macro, default, pem, regex-full, wasm, wasm-bindgen | yes | 37 |
| heddle shipped-heddle-cli | current | 542 | 56 | 35 | biscuit-quote, datalog-macro, default, pem, regex-full | yes | 42 |
| heddle capability-verifier-wasm32 | current | 177 | 34 | 28 | biscuit-quote, datalog-macro, default, pem, regex-full, wasm, wasm-bindgen | yes | 52 |
| heddle workspace-all-targets | fork | 701 | 72 | 35 | wasm, wasm-bindgen | no | 34 |
| heddle shipped-heddle-cli | fork | 539 | 56 | 35 | (none) | no | 39 |
| heddle capability-verifier-wasm32 | fork | 173 | 34 | 28 | wasm, wasm-bindgen | no | 49 |
| weft workspace-all-targets | current | 705 | 72 | 36 | biscuit-quote, datalog-macro, default, pem, regex-full, wasm, wasm-bindgen | yes | 18 |
| weft shipped-weft-server | current | 603 | 56 | 36 | biscuit-quote, datalog-macro, default, pem, regex-full | yes | 20 |
| weft workspace-all-targets | fork-pin | 705 | 72 | 36 | biscuit-quote, datalog-macro, default, pem, regex-full, wasm, wasm-bindgen | yes | 18 |
| weft shipped-weft-server | fork-pin | 603 | 56 | 36 | biscuit-quote, datalog-macro, default, pem, regex-full | yes | 20 |
| weft workspace-all-targets | fork-full | 702 | 72 | 36 | wasm, wasm-bindgen | no | 15 |
| weft shipped-weft-server | fork-full | 600 | 56 | 36 | (none) | no | 17 |

### Builds (seconds; median of 10 after 1 warm-up; configs interleaved)

| Build | Config | Wall p50 | Wall min–max | Wall CV | CPU p50 | CPU CV | Load1 during samples (min–max) |
|---|---|---:|---:|---:|---:|---:|---|
| builds/biscuit-auth-only/clean | current | 13.2 | 7.8–24.1 | 39% | 53.8 | 8% | 8–36 |
| builds/biscuit-auth-only/clean | fork | 8.7 | 7.2–16.8 | 33% | 45.7 | 10% | 8–37 |
| builds/heddle-verifier-leaves/clean | current | 12.9 | 12.4–25.9 | 29% | 85.4 | 6% | 12–18 |
| builds/heddle-verifier-leaves/clean | fork | 12.5 | 12.3–14.3 | 5% | 78.8 | 1% | 12–18 |
| builds/weft-authz/clean | current | 21.8 | 19.2–30.4 | 16% | 187.3 | 4% | 14–24 |
| builds/weft-authz/clean | fork-full | 21.8 | 19.1–28.4 | 15% | 183.9 | 3% | 15–22 |
| builds/heddle-cli/incremental-touch-biscuit-verifier | current | 14.1 | 10.9–18.6 | 17% | 16.6 | 2% | 7–11 |
| builds/heddle-cli/incremental-touch-biscuit-verifier | fork | 12.7 | 11.3–20.5 | 22% | 16.8 | 2% | 7–11 |
| builds/weft-server/incremental-touch-weft-authz | current | 11.9 | 10.0–17.8 | 22% | 15.5 | 9% | 8–15 |
| builds/weft-server/incremental-touch-weft-authz | fork-full | 10.3 | 9.8–12.1 | 8% | 14.6 | 4% | 7–17 |

### Sizes

| Artifact | Config | Bytes | gzip -9 | zstd -19 | brotli 11 |
|---|---|---:|---:|---:|---:|
| sizes/heddle-cli-release | current | 73,483,992 | 29,553,231 | 22,073,026 | 21,317,236 |
| sizes/heddle-cli-release | fork | 73,483,672 | 29,550,692 | 22,091,382 | 21,294,999 |
| sizes/capability-verifier-cdylib-release | current | 422,008 | 224,079 | 207,947 | 190,680 |
| sizes/capability-verifier-cdylib-release | fork | 412,584 | 217,639 | 201,924 | 185,874 |
| sizes/weft-server-production | current | 80,113,040 | 31,287,082 | 22,764,733 | 21,899,205 |
| sizes/weft-server-production | fork-full | 80,119,440 | 31,287,141 | 22,745,865 | 21,894,433 |
| wasm/capability-verifier-release-raw | current | 2,802,600 | 888,944 | 660,042 | 611,058 |
| wasm/capability-verifier-release-raw | fork | 2,135,071 | 684,741 | 522,417 | 487,250 |
| wasm/capability-verifier-wasm-bindgen-web | current | 2,321,102 | 803,517 | 605,012 | 562,472 |
| wasm/capability-verifier-wasm-bindgen-web | fork | 1,674,201 | 597,146 | 467,946 | 437,782 |
| wasm/weft-worker-heddle-iroh-object-provider | current | 6,078,120 | 2,077,570 | 1,490,742 | 1,389,320 |
| wasm/weft-worker-heddle-iroh-object-provider | fork-full | 5,358,552 | 1,846,537 | 1,347,906 | 1,256,073 |

### Latency (µs; pooled over 10 runs after 1 warm-up run)

| Operation | Config | Token bytes | p50 | p95 | p99 | max | run-median min–max |
|---|---|---:|---:|---:|---:|---:|---|
| mint | current | 1,040 | 38 | 47 | 54 | 81 | 38–38 |
| mint | fork | 1,040 | 38 | 41 | 43 | 60 | 37–38 |
| attenuate_one_hop | current | 2,152 | 84 | 95 | 108 | 129 | 83–85 |
| attenuate_one_hop | fork | 2,152 | 84 | 91 | 121 | 178 | 83–84 |
| verify_root | current | 1,040 | 108 | 120 | 140 | 230 | 106–112 |
| verify_root | fork | 1,040 | 108 | 116 | 140 | 201 | 106–112 |
| verify_one_hop | current | 2,152 | 206 | 235 | 282 | 380 | 199–210 |
| verify_one_hop | fork | 2,152 | 202 | 218 | 283 | 340 | 198–209 |
| verify_8_hops | current | 8,996 | 825 | 881 | 970 | 1,227 | 803–862 |
| verify_8_hops | fork | 8,996 | 818 | 851 | 933 | 1,178 | 806–841 |
| verify_16_hops | current | 16,932 | 1,540 | 1,635 | 1,762 | 2,375 | 1,512–1,619 |
| verify_16_hops | fork | 16,932 | 1,542 | 1,608 | 1,810 | 2,032 | 1,514–1,578 |
| verify_max_hops | current | 64,548 | 6,251 | 6,686 | 7,163 | 8,407 | 6,098–6,595 |
| verify_max_hops | fork | 64,548 | 6,224 | 6,468 | 6,667 | 7,495 | 6,155–6,435 |
| verify_worst_wide_caveats | current | 65,368 | 11,910 | 12,789 | 13,491 | 14,679 | 11,835–12,702 |
| verify_worst_wide_caveats | fork | 65,368 | 11,606 | 12,428 | 13,473 | 16,211 | 11,485–12,360 |
| verify_worst_many_rights | current | 23,632 | 1,974 | 2,088 | 2,802 | 2,965 | 1,950–2,016 |
| verify_worst_many_rights | fork | 23,632 | 1,976 | 2,036 | 2,177 | 2,719 | 1,957–1,997 |

Bench `current`: shape {"max_hops": 64, "max_hops_stop": "next hop would be 65540 base64 bytes > 65536", "wide_caveat_entries": 274, "max_rights_facts": 490, "root_bytes": 1040, "one_hop_bytes": 2152, "max_hop_bytes": 64548, "wide_bytes": 65368, "rights_bytes": 23632}; binary 3,193,344 bytes; `biscuit-auth v6.0.0|biscuit-quote,datalog-macro,default,pem,regex-full`

Bench `fork`: shape {"max_hops": 64, "max_hops_stop": "next hop would be 65540 base64 bytes > 65536", "wide_caveat_entries": 274, "max_rights_facts": 490, "root_bytes": 1040, "one_hop_bytes": 2152, "max_hop_bytes": 64548, "wide_bytes": 65368, "rights_bytes": 23632}; binary 2,184,040 bytes; `biscuit-auth v6.0.0 (https://github.com/eclipse-biscuit/biscuit-rust?rev=a6b72596ebe5f391b60e9b91c74edca8febdda93#a6b72596)|`

### Negative tests

| Test | Expect | Demonstrated | Evidence |
|---|---|---|---|
| negative/heddle/biscuit-6.0.0-no-macro-fails | fail | True | ['error: could not compile `biscuit-auth` (lib) due to 5 previous errors', 'error[E0432]: unresolved import `super::ToAnyParam`'] |
| negative/heddle/pin-no-macro-builds-native | pass | True |  |
| negative/heddle/pin-no-macro-builds-wasm32 | pass | True |  |
| negative/heddle/wasm-feature-kept-passes | pass | True | ['test result: ok. 18 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.68s'] |
| negative/heddle/wasm-feature-removed-fails | fail | True | ['error: could not compile `biscuit-auth` (lib) due to 3 previous errors', 'error[E0425]: cannot find function `performance_now` in this scope', 'error[E0599]:  |
| negative/weft/verify | pass | True | 14/14 checks pass |
| negative/report-missing-metadata-rejected | fail | True |  |

## Feature needs (exact)

| Feature | Needed? | Evidence |
|---|---|---|
| `datalog-macro` | Production code: **no**. Tests: **one Weft integration test**. | The only importer is `weft/crates/weft-hosted/tests/v2_replication/owner_management.rs` (4 `fact!` uses). With it removed, that test fails to compile (`E0432 unresolved import biscuit_auth::macros`). As a weft-hosted dev-dependency it compiles, and it stays out of the shipped `weft-server` graph (Weft `--verify`, 14/14). The Heddle workspace, all targets, compiles with no biscuit features on the pin. |
| `regex-full` | No usage found | There is no `.matches(` in any emitted Datalog in Heddle or Weft. Heddle `heddle-biscuit-verifier` and `heddleco-capability-verifier` tests pass on the fork (59+1+41+1 passed, 1 ignored), as do Weft `weft-authz` lib and tests (186+9 passed) under both current and fork-full. |
| `pem` | No usage found | No Biscuit PEM/DER key loading in Heddle or Weft. The compile and test results are the same as the row above. |
| `wasm` | **Yes, on wasm32** | Without it, the pinned `biscuit-auth` fails to compile for `wasm32-unknown-unknown` (`E0425 cannot find function performance_now`, `E0599` in `src/time.rs`). With it, all 18 `heddleco-capability-verifier` wasm tests pass under wasm-bindgen-test-runner 0.2.127. |

## Negative tests

| Test | Result |
|---|---|
| `biscuit-auth 6.0.0` with macros disabled | **Fails**: `error[E0432]: unresolved import super::ToAnyParam`. This is the known unconditional import. |
| Pin with macros disabled, native and wasm32 | **Builds** |
| Removing a needed feature (`wasm` on wasm32) | **Fails** (see above) |
| Omitting `datalog-macro` for the real Weft macro test | **Fails**, `E0432` |
| Report missing `meta.toolchain` / `meta.sources` | **Rejected** by `--verify-report` (schema: `'toolchain' is a required property`) |
| Weft: only the pin, published Heddle crates unchanged (`fork-pin`) | `datalog-macro`, `pem` and `regex-full` stay enabled through feature unification. `proc-macro-error2` stays in the graph. |

## Go/no-go table

This table states what was measured. It does not decide D-01.

"Removal upper bound" means what would leave the graph if `biscuit-auth` were removed entirely: the packages reachable only through it. It is **not** a measurement of Apex, which does not exist. Apex's own proposed dependencies (§8.2: `ed25519-dalek 3`, `curve25519-dalek 5`, `sha2 0.11`) are already in both graphs.

| Dimension | Current Biscuit 6.0.0 | Minimal-feature fork (pin) | Fork vs current | Removal upper bound (graph only) |
|---|---|---|---|---|
| **Dependency weight: shipped Heddle `heddle-cli`** | 542 packages; 35 duplicated crypto/protobuf names | 539 | −3 (`biscuit-quote`, `proc-macro-error2`, `proc-macro-error-attr2`); duplicates unchanged at 35 | −39 packages (among them `ed25519-dalek 2`, `p256 0.13`, `ecdsa 0.16`, `prost 0.10`, `sha2 0.9`, `nom 7`, `syn 1`); duplicated crypto/protobuf names 35 → 13 |
| **Dependency weight: shipped Weft `weft-server`** | 603; 36 | 600 (**requires** the three published Heddle crates to drop biscuit defaults; with the pin alone it stays 603) | −3 (same three); duplicates unchanged at 36 | −17 packages (`prost 0.10`, `sha2 0.9`, `digest 0.9`, `nom 7`, `syn 1`, …); duplicated names 36 → 33. Weft keeps the p256 0.13 / ed25519-dalek 2 stacks through other dependencies |
| **Dependency weight: `heddleco-capability-verifier` on wasm32** | 177; 28 | 173 | −4 | −49 packages; duplicated names 28 → 5 |
| **Build time: clean `biscuit-auth` plus its dependency closure** | CPU 53.8 s (wall 13.2 s) | CPU 45.7 s (wall 8.7 s) | CPU −8.1 s (−15%, SD ≈4.5 s) | Not measurable without Apex; 45.7 s CPU is what the pinned crate and its closure cost |
| **Build time: clean Heddle verifier crates** | CPU 85.4 s | CPU 78.8 s | −6.6 s (−8%) | not measured |
| **Build time: clean `weft-authz`** | CPU 187.3 s | CPU 183.9 s | −3.4 s (−2%, within about one SD) | not measured |
| **Build time: incremental** (touch verifier / `weft-authz`, rebuild shipped bin) | Heddle CPU 16.6 s; Weft CPU 15.5 s | 16.8 s; 14.6 s | No measurable difference | not measured |
| **Binary size: `heddle` release** | 73,483,992 B | 73,483,672 B | −320 B (0.0004%) | not measured |
| **Binary size: `weft` production** | 80,113,040 B | 80,119,440 B | +6,400 B (+0.008%) | not measured |
| **Binary size: capability-verifier native cdylib** | 422,008 B | 412,584 B | −9,424 B (−2.2%) | not measured |
| **Wasm: capability-verifier, wasm-bindgen `--target web`** | 2,321,102 B (brotli 562,472) | 1,674,201 B (brotli 437,782) | −646,901 B (−27.9%); brotli −22% | not measured. SPEC §8.3's provisional Apex verifier target is < 128 KiB optimized wasm (a target, not a measurement) |
| **Wasm: Weft Worker (`heddle-iroh-object-provider`)** | 6,078,120 B (brotli 1,389,320) | 5,358,552 B (brotli 1,256,073) | −719,568 B (−11.8%); brotli −9.6% | not measured |
| **Tapestry production bundle** | Client JS 22,985,462 B (brotli 3,819,995, 927 files). The `biscuit.ts` encoder chunk is 15,905 B (brotli 4,506) | same | 0 by construction (no Rust Biscuit in the bundle) | Apex's TS package would replace the ~15.9 KB encoder chunk; SPEC target < 32 KiB compressed (a target) |
| **Latency: mint** (Weft authority shape, 1,040 B) | p50 38 µs · p95 47 · p99 54 | p50 38 · p95 41 · p99 43 | No measurable difference | — |
| **Latency: attenuate one hop** (agent block plus signed PoP, 2,152 B) | p50 84 µs · p95 95 · p99 108 | 84 · 91 · 121 | No measurable difference | — |
| **Latency: verify root / one hop** | 108 / 206 µs p50; p99 140 / 282 | 108 / 202; p99 140 / 283 | No measurable difference | — |
| **Latency: verify max hops** (64 hops, the 64 KiB credential bound) | p50 6.25 ms · p95 6.69 · p99 7.16 | 6.22 · 6.47 · 6.67 | No measurable difference | — |
| **Latency: worst accepted verify found** (1 hop, 274 op + 274 resource caveat entries, 65,368 B) | p50 11.9 ms · p99 13.5 · max 14.7 | 11.6 · 13.5 · 16.2 | No measurable difference | — |
| **Biscuit features needed** | Defaults on: `regex-full`, `datalog-macro`, `pem` (+`wasm` on wasm32) | Production: only `wasm` on wasm32. Tests: `datalog-macro` for one Weft test (dev-dependency) | Removing the rest breaks no compile and no test | — |

The worst-accepted figure is the slowest of the accepted inputs this bench constructed. It covers the maximum hop chain, wide caveat lists, and the maximum rights facts before the verifier's `max_facts = 1000` limit rejects (490 rights, 1.98 ms p50). It is **not** a proven upper bound on verification cost. Verification time scales with hop count at roughly 0.1 ms per hop (0.21 ms at 1 hop, 0.82 at 8, 1.54 at 16, 6.2 at 64).
