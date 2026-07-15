# Releasing Heddle

Heddle has two release pipelines, both static-asserted on every PR:

| Pipeline | Trigger | Workflow | Asserter |
|---|---|---|---|
| **Binary release** — `heddle` CLI archives, the macOS cask DMG, and downstream package-manifest PRs | `vX.Y.Z` tag push (or `workflow_dispatch` for RC dry-runs) | `.github/workflows/release.yml` | `scripts/check-release-pipeline.sh` |
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
     (build, build-macos-cask, release, publish-manifests) checks out
     **that SHA**, not `refs/tags/<tag>`.
     A tag is mutable; force-moving it after `validate-tag` passes
     would otherwise redirect the build to an attacker-controlled
     commit (TOCTOU). The SHA pin keeps every signed artifact tied to
     the commit that passed the ancestry check.

   If `validate-tag` fails, no build, sign, or publish step runs. If it
   passes, the release jobs proceed to:

   - build the non-mac `heddle` binaries natively on Linux/Windows
     GitHub-hosted runners
   - package each into a versioned archive (`.tar.gz` for unix,
     `.zip` for windows)
   - build macOS once in `build-macos-cask`, Developer ID sign and notarize
     both standalone CLI/worker pairs, then package their tarballs and the
     signed, notarized universal DMG containing `Heddle.app`, with the CLI bundled at
     `Heddle.app/Contents/Resources/bin/heddle`, from those same binaries
   - emit a `.sha256` next to each archive
   - sign each archive/DMG with `cosign` keyless (Sigstore public-good
     instance; trust is rooted in the GitHub OIDC token for this run)
   - publish a GitHub Release with auto-generated notes, all
     artifacts, signatures, certificates, and an aggregated
     `SHA256SUMS`
   - for stable releases only, render `Casks/heddle.rb` and open a PR
     against `HeddleCo/homebrew-tap`

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
| `Heddle-v<version>-macos-universal.dmg` | signed + notarized cask artifact containing `Heddle.app` and the bundled CLI |
| `Heddle-v<version>-macos-universal.dmg.sha256` | one-line `<hex>  <filename>` |
| `Heddle-v<version>-macos-universal.dmg.sig` | cosign signature (base64) |
| `Heddle-v<version>-macos-universal.dmg.pem` | cosign certificate (Fulcio-issued) |
| `SHA256SUMS` | aggregated, one line per archive, sorted |

Targets (`<target>`):

- `aarch64-apple-darwin` — macOS arm64 (Apple Silicon)
- `x86_64-apple-darwin` — macOS x64 (Intel)
- `aarch64-unknown-linux-gnu` — Linux arm64 (glibc)
- `x86_64-unknown-linux-gnu` — Linux x64 (glibc)
- `x86_64-pc-windows-msvc` — Windows x64 (MSVC)
- `aarch64-pc-windows-msvc` — Windows arm64 (MSVC)

Each archive contains:

- `heddle` (or `heddle.exe` on Windows) — the CLI binary, release profile
- `heddle-fsmonitor-worker` (or `.exe` on Windows) — the sibling process that keeps repeated status scans incremental
- `README.md`, `LICENSE`, `NOTICE`

The two macOS archives contain Developer ID signed binaries. Both target
pairs are submitted to Apple's notary service before the tarballs are staged;
the tarballs themselves also carry the same cosign release signature as every
other archive.

Downstream channels **must** consume:

- Homebrew cask: the macOS universal DMG URL and sha256
- Scoop / apt / any CLI-only channel: the target archive URL and sha256
- optionally the `.sig` + `.pem` for signature verification

The asset filenames and the `SHA256SUMS` layout are part of this
contract. Changing them is a breaking change for downstream packaging
channels and requires a coordinated update.

## Homebrew cask publication

The primary macOS install path is:

```bash
brew install --cask heddleco/heddle/heddle
```

The tap repository is `HeddleCo/homebrew-tap`, which maps to
`heddleco/tap` by Homebrew's tap naming convention. Stable releases
render `Casks/heddle.rb` with:

- `app "Heddle.app"` so Homebrew installs the host app into
  `/Applications`
- `binary "#{appdir}/Heddle.app/Contents/Resources/bin/heddle",
  target: "heddle"` so the CLI is symlinked into Homebrew's `bin`
- `depends_on macos: ">= :tahoe"` because the native FSKit path relies
  on macOS 26 FSKit APIs

The cask is not pushed directly. The `publish-manifests` job uses a
HeddleCo GitHub App installation token to open or refresh a PR in the
tap repo. A maintainer merges that PR after tap CI passes.

### macOS signing and tap secrets

The macOS cask artifact job expects these GitHub Actions secrets in the
`HeddleCo/heddle` repo:

| Name | Purpose |
|---|---|
| `HEDDLE_DEVELOPER_ID_APPLICATION_CERTIFICATE_BASE64` | Base64-encoded `.p12` for the Developer ID Application certificate |
| `HEDDLE_DEVELOPER_ID_APPLICATION_CERTIFICATE_PASSWORD` | Password for that `.p12` |
| `HEDDLE_HOST_PROVISION_PROFILE_BASE64` | Base64-encoded Developer ID provisioning profile for `sh.heddle.HeddleHost` |
| `HEDDLE_FSMODULE_PROVISION_PROFILE_BASE64` | Base64-encoded Developer ID provisioning profile for `sh.heddle.HeddleHost.HeddleFSModule` |
| `HEDDLE_NOTARY_PRIVATE_KEY` | App Store Connect API private key text |
| `HEDDLE_NOTARY_KEY_ID` | App Store Connect key ID |
| `HEDDLE_NOTARY_ISSUER_ID` | App Store Connect issuer ID |
| `HEDDLE_RELEASE_APP_ID` | HeddleCo release-publisher GitHub App ID |
| `HEDDLE_RELEASE_APP_PRIVATE_KEY` | Private key for the release-publisher GitHub App |

Set these GitHub Actions variables as well:

| Name | Purpose |
|---|---|
| `HEDDLE_TEAM_ID` | Apple Developer team ID; defaults to `33V6242M8S` in the workflow |
| `HEDDLE_DEVELOPER_ID_APPLICATION` | Codesigning identity, e.g. `Developer ID Application: HeddleCo, LLC (33V6242M8S)` |

The release-publisher GitHub App should be installed only on
`homebrew-tap`, `scoop-heddle`, and `apt-heddle` and granted only
`Contents: write` and `Pull requests: write`. The token minted in the
`publish-manifests` job lists all three in its `repositories:` scope.

## Scoop manifest publication

The Windows install path is:

```bash
scoop bucket add heddle https://github.com/HeddleCo/scoop-heddle
scoop install heddle
```

The bucket repository is `HeddleCo/scoop-heddle`; Scoop reads manifests
from its `bucket/` directory. Stable releases render `bucket/heddle.json`
from the Windows zip line(s) in the release `SHA256SUMS` via
`scripts/render-scoop-manifest.sh`. The manifest declares:

- `architecture.64bit.url` / `.hash` — the `x86_64-pc-windows-msvc.zip`
  archive and its sha256
- `bin` / `shortcuts` pointing at `heddle.exe` inside the extracted
  archive directory
- `checkver` + `autoupdate` so the bucket can self-update to future
  stable tags
- `notes` with the cosign keyless verification command (the `.sig` +
  `.pem` are published alongside the archive on the GitHub Release)

Only **x64** is shipped today. `aarch64-pc-windows-msvc` is parked
because `sigstore/cosign-installer@v3` has no Windows-arm64 asset, so the
release build matrix omits it (see the matrix comment in `release.yml`
and #347). When that target re-enters the matrix, add an
`aarch64-pc-windows-msvc` row to `ARCHES` in the renderer and Scoop picks
up the new architecture block automatically.

Like the Homebrew cask, the manifest is not pushed directly: the
`publish-manifests` job uses the same release-publisher App token to open
or refresh a PR against `scoop-heddle`, which a maintainer merges after
bucket CI passes. The wiring (renderer present, `bucket/heddle.json`
path, App token, `HeddleCo/scoop-heddle` target) is asserted by
`scripts/check-release-pipeline.sh`.

## apt repository publication

The Debian/Ubuntu install path pins Heddle's signing key to its own
keyring and scopes trust to Heddle's source with `signed-by=` (never
`apt-key add`, which is removed on modern apt):

```bash
# 1. Install Heddle's signing key into its own keyring.
curl -fsSL https://apt.heddle.sh/heddle-archive-keyring.gpg \
  | sudo tee /usr/share/keyrings/heddle-archive-keyring.gpg > /dev/null

# 2. Register the source, pinned to that keyring + this machine's arch.
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/heddle-archive-keyring.gpg] https://apt.heddle.sh stable main" \
  | sudo tee /etc/apt/sources.list.d/heddle.list > /dev/null

# 3. Install. The heddle-archive-keyring package keeps the key current
#    across future `apt upgrade`s.
sudo apt update && sudo apt install heddle
```

The modern deb822 form (`/etc/apt/sources.list.d/heddle.sources`) is
equivalent:

```
Types: deb
URIs: https://apt.heddle.sh
Suites: stable
Components: main
Architectures: amd64 arm64
Signed-By: /usr/share/keyrings/heddle-archive-keyring.gpg
```

The repository is `HeddleCo/apt-heddle` (git-backed pool + signed index,
served as static files via GitHub Pages at `apt.heddle.sh`). Stable
releases run `scripts/build-apt-pool.sh`, which:

- verifies the two `*-unknown-linux-gnu.tar.gz` archives against the
  release `SHA256SUMS`, then builds `heddle_<version>_amd64.deb` and
  `heddle_<version>_arm64.deb` into `pool/main/h/heddle/`
- builds the `heddle-archive-keyring_<n>_all.deb` self-updating trust
  anchor (it ships the dearmored public key into
  `/usr/share/keyrings/`), and writes the same key to
  `heddle-archive-keyring.gpg` at the repo root for manual installers
- generates the `dists/stable/main/binary-<arch>/Packages{,.gz}` indices
  and the suite `dists/stable/Release` (single rolling `stable main`
  suite carrying both arches — the binaries are glibc-dynamic, not
  codename-specific)
- GPG-signs `Release` (detached `Release.gpg` + inline `InRelease`) with
  the Ed25519 signing subkey imported into an ephemeral `GNUPGHOME`. The
  subkey comes from the `HEDDLE_APT_GPG_PRIVATE_KEY` secret; `apt-heddle`
  holds no secrets.

Like the Homebrew cask and Scoop manifest, the signed tree is not pushed
directly: the `publish-manifests` job uses the same release-publisher App
token (scoped to include `apt-heddle`) and the shared
`./.github/actions/publish-manifest` composite action to open a PR
against `apt-heddle` carrying the whole pool + signed index
(`manifest-path: .`). A maintainer merges it; GitHub Pages then serves
the merged tree. The wiring (`scripts/build-apt-pool.sh` present, the
GPG secret imported into an ephemeral `GNUPGHOME`, the `apt-heddle`
target, and the widened App-token scope) is asserted by
`scripts/check-release-pipeline.sh`.

The design rationale — hosting platform, GPG key strategy, key rotation,
and install UX — lives in
`docs/design/apt-hosting-gpg-spike.md`.

### Required GitHub Actions secrets (apt)

- `HEDDLE_APT_GPG_PRIVATE_KEY` — the armored Ed25519 **signing subkey**
  exported offline from the primary. The primary stays offline; only the
  subkey reaches CI, so a leak is revocable without re-minting subscriber
  trust. Rotate by updating this one secret (no workflow change).

### apt human-infra prerequisites (org-admin)

These are one-time setup steps tracked in
`docs/design/apt-hosting-gpg-spike.md`. Until they land, the apt leg
opens its PR but the published repo is not yet reachable at the branded
URL:

1. Create `HeddleCo/apt-heddle`; install the "Heddle Release Publisher"
   GitHub App on it (`Contents: write` + `Pull requests: write`).
2. `apt.heddle.sh` DNS (CNAME → `heddleco.github.io`) + Pages
   custom-domain + TLS.
3. Offline-generate the Ed25519 primary + signing subkey; export the
   subkey to the `HEDDLE_APT_GPG_PRIVATE_KEY` secret.

### Linux glibc floor

The two `-unknown-linux-gnu` legs target **glibc ≥ 2.35**. They are
built on `ubuntu-22.04` / `ubuntu-22.04-arm` runners (glibc 2.35), so
the binaries dynamically link against no symbol newer than `GLIBC_2.35`
and run on Debian 12 (glibc 2.36), Ubuntu 22.04, and every newer glibc
distro forward.

This is a declared contract, not an accident of the runner image.
Building these legs on a newer runner (e.g. `ubuntu-24.04`, glibc 2.39)
raises the symbol floor and crashes the binary at first run on the
supported targets with `GLIBC_2.3x not found` (#549) — it `apt
install`s / extracts fine and only fails on exec, so it is not caught
by packaging tests. The runner pins are asserted by
`scripts/check-release-pipeline.sh`; a bump back to a newer runner
fails that check. To verify a built binary's floor:

```bash
objdump -T heddle | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1   # must be <= GLIBC_2.35
# end-to-end smoke on the oldest supported target:
docker run --rm -v "$PWD:/h" debian:12 /h/heddle --version
```

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

We build natively rather than cross-compiling from a single host, but
macOS is intentionally consolidated into one job: `build-macos-cask`
builds both Apple targets once, packages the standalone CLI tarballs, and
Developer ID signs and notarizes the binaries in those tarballs before
signing/notarizing the universal app DMG from the same build outputs.
Trade-off:

- **Targeted native jobs (chosen)**: Linux/Windows use a small parallel
  matrix, while Apple artifacts are built once on `macos-26`. No `cross`,
  no sysroot juggling, no duplicate macOS CLI builds. Linux ARM stays on
  `ubuntu-22.04-arm` for the glibc floor; macOS stays on `macos-26` so
  the standalone tarballs and cask app use the same FSKit SDK/deployment
  floor.
- **Cross-compilation**: one runner, more setup. Wins on cost only if
  we hit a parallelism cap, which we won't at our release cadence.

Revisit if release frequency increases by an order of magnitude or if
GitHub-hosted runner availability degrades. Cross-compiling macOS binaries
from Linux would move Developer ID signing and notarization into a separate
Apple-hosted stage, adding complexity without reducing the current Apple build.

## Pipeline-contract check

A lightweight `release-pipeline-check` job runs on every PR. It
checks `.github/workflows/release.yml` and `RELEASING.md` in two
passes:

- **Smoke (grep).** Cheap content checks: the five target triples, the
  strict-semver push trigger, presence of the `validate-tag` trust gate
  with its ancestry check, packaging/checksum/signing/upload steps, the
  macOS cask DMG job, the macOS archive ownership by that cask job, the
  Homebrew cask PR wiring, the draft+prerelease keying off
  `validate-tag.outputs.kind`, and the stable-tag-refusal on the dispatch
  path.
- **Strict (parsed YAML).** Per-job structural checks: `validate-tag`
  exports `tag_sha`, and every downstream job (`build`,
  `build-macos-cask`, `release`, `publish-manifests`) both declares
  `needs: validate-tag` and pins its `actions/checkout` `ref`
  to `${{ needs.validate-tag.outputs.tag_sha }}` rather than the
  mutable `refs/tags/<tag>`. Grep alone would pass if *any* job kept
  the `needs:` line; the parser confirms each downstream job
  individually.

The contract above is the contract it enforces. If you intentionally
change the contract, update `scripts/check-release-pipeline.sh` in
the same PR.

## Automated crates.io publishing

The Heddle-owned OSS workspace crates publish to
crates.io automatically on every push to `main` via
`.github/workflows/publish-crates.yml`. The normal flow is:

The public protobuf contract and its `heddle-api` Rust package must be released
independently from `HeddleCo/api`; they are not part of this workspace's
publication pipeline.

The v1alpha1 cutover must not merge while Heddle still relies on the temporary
git pin to API revision `e3b3e6d0`. The TypeScript package
`@heddleco/api@0.1.1` is published, but the Rust `heddle-api@0.1.1` crate is not:
HeddleCo/api#10 is blocked on configuring that repository's
`CARGO_REGISTRY_TOKEN`. Publish the Rust crate and replace the temporary git pin
before the next Heddle release-plz publication. The full cross-repository
release gate is tracked in
[ADR 0048](docs/adr/0048-net-new-public-api-contract.md#coordinated-cutover-checklist).

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
the explicit list keeps the publication scope visible in diff.

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
curl -s https://crates.io/api/v1/crates/heddle-wire | jq '.crate.max_stable_version'
```

The workflow's "Published to crates.io" summary table is the
canonical receipt of what shipped on a given run.
