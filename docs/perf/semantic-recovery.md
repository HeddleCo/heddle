# Semantic recovery sidecar benchmark

Status: first slice implemented behind the `heddle-repo/semantic-recovery`
feature. This slice provides explicit full rebuilds and thread reconstruction;
it does not yet provide a CLI, incremental maintenance, or related-attempt
search.

## Chosen model and index

The [heddle#1201 investigation](https://github.com/HeddleCo/heddle/issues/1201)
selected `BAAI/bge-small-en-v1.5` (384 dimensions) and measured two-stage
residual quantization at 95.2% recovery with 32 coarse and 16 residual
centroids. The first slice implements that exact 32+16, 9-bit default and keeps
48+8 and 32+32 in the benchmark sweep.

The runtime artifact is pinned to:

- repository: `qdrant/bge-small-en-v1.5-onnx-q`
- revision: `52398278842ec682c6f32300af41344b1c0b0bb2`
- artifact: `model_optimized.onnx`
- SHA-256: `51f1bd0addd6e859e42c2c8021a5e5461385bb676a649f4b269aa445449f2431`

`BgeSmallEmbedder` reads the ONNX and tokenizer assets from a caller-supplied
local directory and verifies that digest before initialization. The crate has
no model-hub feature enabled, so inference has no network dependency. State
documents use the investigation's strict UTF-8/control-character filter and
average at most four evenly spaced 800-character chunks before L2
normalization.

The sidecar lives at `.heddle/indexes/semantic-recovery-v1.bin`. It contains a
checksummed header, model identity, codebooks, bit-packed residual codes, state
keys, and thread labels. It does not contain canonical objects or change their
identities. Writes are atomic. A missing sidecar makes reconstruction return no
result, while deleting and rebuilding it leaves every durable object byte
unchanged; the repository integration test asserts both properties.

## Reproduce

Place the five pinned model files in one local directory:

```bash
revision=52398278842ec682c6f32300af41344b1c0b0bb2
model_dir=/tmp/heddle-bge-small
mkdir -p "$model_dir"
for file in model_optimized.onnx tokenizer.json config.json special_tokens_map.json tokenizer_config.json; do
  curl --fail --location --output "$model_dir/$file" \
    "https://huggingface.co/qdrant/bge-small-en-v1.5-onnx-q/resolve/$revision/$file"
done
sha256sum "$model_dir/model_optimized.onnx"
HEDDLE_BGE_SMALL_MODEL_DIR="$model_dir" \
  cargo bench -p heddle-semantic-recovery --bench recovery
```

The fixture has 18 semantically distinct threads and seven states per thread:
baseline, rename, reorder, insert, comment churn, rename+reorder, and mixed. No
thread label is embedded in the state text. Each state is queried with itself
excluded from the candidates.

## Result

Measured on 2026-08-12 with the pinned ONNX artifact:

| Index | Theoretical bits/vector | Packed bits/vector | Thread hits | Thread hit-rate | Sibling recall@6 | Sibling-oracle gap |
|---|---:|---:|---:|---:|---:|---:|
| Full-float oracle | — | — | 126/126 | 100.00% | 83.07% | — |
| RQ 48+8 | 8.58 | 9 | 126/126 | 100.00% | 83.20% | -0.13 pp |
| **RQ 32+16** | **9.00** | **9** | **126/126** | **100.00%** | **79.76%** | **3.31 pp** |
| RQ 32+32 | 10.00 | 10 | 126/126 | 100.00% | 83.47% | -0.40 pp |

The 32+16 thread hit-rate is 18/18 (100%) in every divergence class. The
benchmark fails below a 95% thread-reconstruction floor.

`Thread hit-rate` is the end-to-end contract: the strongest other state must
identify the correct thread, after which the API returns indexed siblings from
that reconstructed thread. `Sibling recall@6` is a deliberately stricter
diagnostic: it asks how many of all six same-thread states occupy the first six
raw global-neighbor positions. The latter is not the API contract, but reporting
it makes ranking loss visible. The finite fixture produced 100%, slightly above
the investigation's 95–99% range; this is reported as measured rather than
rounded down.

## Deferred

This first slice deliberately leaves related-attempt surfacing to
[#1346](https://github.com/HeddleCo/heddle/issues/1346), incremental maintenance
and stale-index detection to
[#1347](https://github.com/HeddleCo/heddle/issues/1347), production model
distribution, CLI, and performance work to
[#1348](https://github.com/HeddleCo/heddle/issues/1348), and broader
real-repository evaluation to
[#1349](https://github.com/HeddleCo/heddle/issues/1349). Shared structural and
statistical chunking remains tracked by
[#1203](https://github.com/HeddleCo/heddle/issues/1203).
