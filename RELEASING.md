# Releasing Heddle

This document covers the binary-release pipeline that produces pre-built
`heddle` CLI artifacts on tagged releases. It is the upstream contract
that the HomeBrew (heddle#29b), Scoop, and apt (heddle#29c) packaging
channels consume.

For the crates.io publishing flow (`heddle-cli` and the workspace crates
managed by `release-plz`), see the existing `release-plz.toml` and the
manual `publish-*.sh` scripts at the repo root. That is a separate
pipeline from the one described here.

## Cutting a release

1. Land your change on `main` (CI green).
2. Tag from `main`:

   ```bash
   git tag -a v0.3.0 -m 'heddle v0.3.0'
   git push origin v0.3.0
   ```

3. The `Release binaries` workflow (`.github/workflows/release.yml`)
   triggers on the `v*` tag push. It:

   - builds the `heddle` binary natively on five GitHub-hosted runners
   - packages each into a versioned archive (`.tar.gz` for unix,
     `.zip` for windows)
   - emits a `.sha256` next to each archive
   - signs each archive with `cosign` keyless (Sigstore public-good
     instance; trust is rooted in the GitHub OIDC token for this run)
   - publishes a GitHub Release with auto-generated notes, all
     artifacts, signatures, certificates, and an aggregated
     `SHA256SUMS`

4. Verify the Release page lists the expected asset set (see
   [Artifact contract](#artifact-contract) below). If anything is
   missing, the upload step fails the workflow — there is no partial
   release.

### Dry-runs

The workflow also accepts `workflow_dispatch` with a `tag` input. Push
a pre-release tag (e.g. `v0.3.0-rc1`), trigger the workflow from the
Actions tab, and inspect the artifacts it produces against a draft or
prerelease GitHub Release. Delete the rc tag and assets when done.

## Artifact contract

For tag `v<version>`, the release publishes one set per target:

| File | Notes |
|---|---|
| `heddle-v<version>-<target>.{tar.gz,zip}` | the archive |
| `heddle-v<version>-<target>.{tar.gz,zip}.sha256` | one-line `<hex>  <filename>` |
| `heddle-v<version>-<target>.{tar.gz,zip}.sig` | cosign signature (base64) |
| `heddle-v<version>-<target>.{tar.gz,zip}.pem` | cosign certificate (Fulcio-issued) |
| `SHA256SUMS` | aggregated, one line per archive, sorted |

Targets (`<target>`):

- `aarch64-apple-darwin` — macOS arm64 (Apple Silicon)
- `x86_64-apple-darwin` — macOS x64 (Intel)
- `aarch64-unknown-linux-gnu` — Linux arm64 (glibc)
- `x86_64-unknown-linux-gnu` — Linux x64 (glibc)
- `x86_64-pc-windows-msvc` — Windows x64 (MSVC)

Each archive contains:

- `heddle` (or `heddle.exe` on Windows) — the CLI binary, release profile
- `README.md`, `LICENSE`, `NOTICE`

Downstream channels (HomeBrew formula, Scoop manifest, apt `.deb`
metadata) **must** consume:

- the archive URL and its `.sha256` for integrity
- optionally the `.sig` + `.pem` for signature verification

The asset filenames and the `SHA256SUMS` layout are part of this
contract. Changing them is a breaking change for downstream packaging
channels and requires a coordinated update.

## Verifying a release

```bash
TAG=v0.3.0
TARGET=aarch64-apple-darwin
URL="https://github.com/HeddleCo/heddle/releases/download/${TAG}"
ARCHIVE="heddle-${TAG}-${TARGET}.tar.gz"

curl -fSLO "${URL}/${ARCHIVE}"
curl -fSLO "${URL}/${ARCHIVE}.sha256"
curl -fSLO "${URL}/${ARCHIVE}.sig"
curl -fSLO "${URL}/${ARCHIVE}.pem"

# Integrity.
shasum -a 256 -c "${ARCHIVE}.sha256"

# Signature (cosign keyless). The certificate identity is the workflow
# file that issued it; the issuer is GitHub Actions OIDC.
cosign verify-blob \
  --certificate "${ARCHIVE}.pem" \
  --signature   "${ARCHIVE}.sig" \
  --certificate-identity-regexp 'https://github\.com/HeddleCo/heddle/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  "${ARCHIVE}"
```

## Build strategy: native matrix vs. cross-compilation

We build natively (one GitHub-hosted runner per target) rather than
cross-compiling from a single host. Trade-off:

- **Native matrix (chosen)**: five parallel runners (~5–10 min each
  with `Swatinem/rust-cache`). No `cross`, no sysroot juggling, no
  Apple-codesign-on-Linux contortions later if/when we add notarization.
  ARM is free on GitHub-hosted runners (`ubuntu-24.04-arm`, `macos-14`).
- **Cross-compilation**: one runner, more setup. Wins on cost only if
  we hit a parallelism cap, which we won't at our release cadence.

Revisit if release frequency increases by an order of magnitude, if
GitHub-hosted runner availability degrades, or when we add macOS
notarization (cross-compiling macOS binaries from Linux makes the
codesign + notarize step substantially harder and was a real cost in
similar projects).

## Pipeline-contract check

A lightweight `release-pipeline-check` job runs on every PR. It greps
`.github/workflows/release.yml` for the five target triples, the
tag-push trigger, packaging, checksum, signing, and upload steps, and
greps `RELEASING.md` for each target. The contract above is the
contract it enforces. If you intentionally change the contract,
update `scripts/check-release-pipeline.sh` in the same PR.
