# Releasing Heddle

Heddle has two release pipelines, both static-asserted on every PR:

| Pipeline | Trigger | Workflow | Asserter |
|---|---|---|---|
| **Binary release** — `heddle` CLI artifacts for HomeBrew / Scoop / apt | `vX.Y.Z` tag push (or `workflow_dispatch` for RC dry-runs) | `.github/workflows/release.yml` | `scripts/check-release-pipeline.sh` |
| **crates.io publish** — workspace crates managed by `release-plz` | push to `main` (typically a release-plz merge) | `.github/workflows/publish-crates.yml` | `scripts/check-publish-pipeline.sh` |

The two are independent — a binary release doesn't bump crate versions
and a crates.io publish doesn't produce binaries — but they follow the
same trust pattern: a `validate-*` job runs first and exposes a SHA
output; every downstream credentialed step pins `actions/checkout` to
that SHA rather than the mutable ref it was triggered on. See the
"Pipeline-contract check" sections below for what the asserters enforce.

The manual `publish-*.sh` scripts at the repo root are kept as the
fallback path for bootstrap publishes (and for the 0.2.0 cutover that
predated the workflow). For routine version bumps, prefer the
release-plz → push-to-main flow documented in
[Automated crates.io publishing](#automated-cratesio-publishing) below.

## Cutting a release

1. Land your change on `main` (CI green).
2. Tag the commit you want to release from `main`. Tags **must** match
   strict semver (`vX.Y.Z`); any other shape will not trigger the
   release workflow on push:

   ```bash
   git tag -a v0.3.0 -m 'heddle v0.3.0'
   git push origin v0.3.0
   ```

3. The `Release binaries` workflow (`.github/workflows/release.yml`)
   triggers on the stable-semver tag push. Before building anything it
   runs a `validate-tag` gate that:

   - resolves the requested tag from the trigger (push or dispatch)
   - rejects refs that aren't real tags (catches `main`, typos, deleted
     tags fed to `workflow_dispatch`)
   - rejects tags whose commit isn't reachable from `origin/main`
     (catches tags accidentally — or maliciously — placed on a feature
     branch)
   - rejects stable (`vX.Y.Z`) tags fed to `workflow_dispatch`. Stable
     releases must arrive via the push trigger; dispatch is the
     prerelease/dry-run path only. See [Dry-runs](#dry-runs) for why.
   - classifies the run as `stable` or `prerelease`
   - emits the resolved commit SHA as `tag_sha`. Every downstream job
     (build, release) checks out **that SHA**, not `refs/tags/<tag>`.
     A tag is mutable; force-moving it after `validate-tag` passes
     would otherwise redirect the build to an attacker-controlled
     commit (TOCTOU). The SHA pin keeps every signed artifact tied to
     the commit that passed the ancestry check.

   If `validate-tag` fails, no build, sign, or publish step runs. If it
   passes, the matrix proceeds to:

   - build the `heddle` binary natively on five GitHub-hosted runners
   - package each into a versioned archive (`.tar.gz` for unix,
     `.zip` for windows)
   - emit a `.sha256` next to each archive
   - sign each archive with `cosign` keyless (Sigstore public-good
     instance; trust is rooted in the GitHub OIDC token for this run)
   - publish a GitHub Release with auto-generated notes, all
     artifacts, signatures, certificates, and an aggregated
     `SHA256SUMS`

4. Verify the Release page lists the expected asset set (see
   [Artifact contract](#artifact-contract) below). If anything is
   missing, the upload step fails the workflow — there is no partial
   release.

### Dry-runs

Pre-release tags (`-rc`, `-beta`, `-alpha`) intentionally do **not**
fire the push trigger — only `vX.Y.Z` does. To rehearse a release:

1. Push an RC tag from `main`:

   ```bash
   git tag -a v0.3.0-rc.1 -m 'heddle v0.3.0-rc.1'
   git push origin v0.3.0-rc.1
   ```

   (This push alone does not run the workflow.)

2. From the Actions tab, run `Release binaries` via `workflow_dispatch`
   with `tag: v0.3.0-rc.1`.

3. The run goes through `validate-tag` exactly as a real release would.
   On publish, the GitHub Release is created as **draft + prerelease**.
   Inspect the draft release, then delete the draft release and the RC
   tag/assets when done.

Accepted tag patterns:

| Trigger | Accepted | Rejected |
|---|---|---|
| `push` (tag) | `vX.Y.Z` | everything else (push filter is strict) |
| `workflow_dispatch` | `vX.Y.Z-(rc\|alpha\|beta)[.N]` | `vX.Y.Z` (stable), anything else |

Stable tags (`vX.Y.Z`) are deliberately refused on the dispatch path.
The dispatch path always classifies the run as `kind=prerelease+draft`,
and `softprops/action-gh-release` updates an existing release when its
`tag_name` already matches — so dispatching a previously-published
stable tag would silently overwrite the public release with a
draft/prerelease shell. Refusing the combination in `validate-tag`
makes that downgrade attack syntactically impossible.

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

A lightweight `release-pipeline-check` job runs on every PR. It
checks `.github/workflows/release.yml` and `RELEASING.md` in two
passes:

- **Smoke (grep).** Cheap content checks: the five target triples, the
  strict-semver push trigger, presence of the `validate-tag` trust gate
  with its ancestry check, packaging/checksum/signing/upload steps, the
  draft+prerelease keying off `validate-tag.outputs.kind`, and the
  stable-tag-refusal on the dispatch path.
- **Strict (parsed YAML).** Per-job structural checks: `validate-tag`
  exports `tag_sha`, and every downstream job (`build`, `release`) both
  declares `needs: validate-tag` and pins its `actions/checkout` `ref`
  to `${{ needs.validate-tag.outputs.tag_sha }}` rather than the
  mutable `refs/tags/<tag>`. Grep alone would pass if *any* job kept
  the `needs:` line; the parser confirms each downstream job
  individually.

The contract above is the contract it enforces. If you intentionally
change the contract, update `scripts/check-release-pipeline.sh` in
the same PR.

## Automated crates.io publishing

`heddle-grpc` and the rest of the OSS workspace crates publish to
crates.io automatically on every push to `main` via
`.github/workflows/publish-crates.yml`. The normal flow is:

1. `release-plz` (configured in `release-plz.toml`) opens a PR that
   bumps Cargo.toml versions and updates `CHANGELOG.md`.
2. A maintainer reviews and merges the PR.
3. On the resulting push to `main`, `publish-crates.yml` runs:
   - `validate-publish` confirms the push is on `main`, captures the
     merged commit SHA as `commit_sha`, and probes crates.io for
     each declared-publishable crate. For each it emits one of:
     **publish** (Cargo.toml version isn't on crates.io yet),
     **skip** (already published — idempotent re-run), or **fail**
     (Cargo.toml downgrade — refuses).
   - `publish` runs only when `has_publishes == 'true'`, checks out
     the validated `commit_sha` (not `refs/heads/main` — see the
     TOCTOU note in `release.yml`), asserts the `CARGO_REGISTRY_TOKEN`
     env var is non-empty (sourced from `secrets.CRATES_IO_API_KEY` —
     see [Token wiring](#token-wiring) below), and runs
     `cargo publish -p <crate>` for each entry in the publish set.
     "already exists" errors are treated as success (race / re-run);
     5xx errors retry with exponential backoff (1s → 4s → 16s);
     anything else fails loud.
   - A workflow run summary lists each published `<crate>@<version>`
     with a crates.io link.

### Trigger choice

The workflow fires only on `on.push.branches: ['main']`. There is no
`workflow_dispatch` path — automation must never be triggerable from
outside `main`'s history. If a maintainer needs to force-publish (a
bootstrap, a recovery), they run `cargo publish` locally with their
own creds; that's a deliberate ops action, not workflow surface.

### Publishable crate list

Maintained as an explicit `PUBLISHABLE_CRATES` env var in
`publish-crates.yml`, in topological order (deps first). Adding a new
publishable crate is a one-line workflow edit, reviewed in PR. The
list mirrors `release-plz.toml`'s `[[package]]` blocks.

Auto-discovery (`cargo metadata --workspace`) is deliberately avoided:
an implicit `publish = true` (or absence of `publish = false`) in a
new Cargo.toml is invisible at PR review time, and accidentally
flipping it would silently expand the public surface. Currently
**all 17 workspace crates are publishable** (none declare
`publish = false`); the explicit list keeps that scope visible in
diff.

### Token wiring

The workflow's publish job exposes the credential to cargo via:

```yaml
env:
  CARGO_REGISTRY_TOKEN: ${{ secrets.CRATES_IO_API_KEY }}
```

The two names are deliberately distinct halves of the mapping:

- `CARGO_REGISTRY_TOKEN` is the env-var name `cargo publish` reads at
  runtime (cargo's documented name). Renaming this side would mean
  cargo can't find the token at all.
- `CRATES_IO_API_KEY` is the GitHub Actions secret name as configured
  under repo Settings → Secrets and variables → Actions. Renaming this
  side would resolve to an empty string and break authentication on
  the first publish.

The asserter (see below) checks both halves separately so a regression
on either side surfaces with its own error line.

To rotate the token: update the `CRATES_IO_API_KEY` secret in repo
settings. No workflow change is needed.

### Pipeline-contract check

`scripts/check-publish-pipeline.sh` runs alongside the binary
release check on every PR (via `release-pipeline-check.yml`). Same
two-pass shape as `check-release-pipeline.sh`:

- **Smoke (grep).** push-to-main trigger present, `workflow_dispatch`
  absent, `validate-publish` + `publish` jobs both present, publish
  job declares `needs: validate-publish`, `secrets.CRATES_IO_API_KEY`
  and the `CARGO_REGISTRY_TOKEN` env var both referenced, explicit
  `PUBLISHABLE_CRATES` list present, this section exists in
  `RELEASING.md`.
- **Strict (parsed YAML).** `validate-publish` exports `commit_sha`,
  `to_publish`, `has_publishes`; `publish` declares
  `needs: validate-publish` and gates `if:` on `has_publishes`;
  publish's `actions/checkout` pins `ref` to
  `${{ needs.validate-publish.outputs.commit_sha }}` (not
  `refs/heads/main` — TOCTOU); the env-var key is exactly
  `CARGO_REGISTRY_TOKEN` (cargo's documented name); that env var is
  wired from `secrets.CRATES_IO_API_KEY` (the repo-settings secret
  name).

### Verifying a publish

```bash
# After a release-plz PR merges, watch the workflow:
gh run watch --repo HeddleCo/heddle --workflow publish-crates.yml

# Once green, confirm the crate is queryable:
curl -s https://crates.io/api/v1/crates/heddle-grpc | jq '.crate.max_stable_version'
```

The workflow's "Published to crates.io" summary table is the
canonical receipt of what shipped on a given run.
