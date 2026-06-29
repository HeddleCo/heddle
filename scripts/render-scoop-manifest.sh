#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/render-scoop-manifest.sh <tag> <SHA256SUMS> <output>

Renders the Heddle Scoop manifest for a stable GitHub Release.

  tag         Release tag, e.g. v0.3.0
  SHA256SUMS  Aggregate checksum file containing the Windows zip line(s)
  output      Destination manifest path (bucket/heddle.json)

The Windows release ships x64 only today (aarch64-pc-windows-msvc is
parked until cosign publishes a win-arm64 binary; see release.yml and
the artifact contract in RELEASING.md). When the arm64 leg lands, add an
aarch64-pc-windows-msvc block to ARCHES below and Scoop picks it up
automatically.
USAGE
}

if [[ $# -ne 3 ]]; then
  usage
  exit 2
fi

TAG="$1"
SHA256SUMS="$2"
OUTPUT="$3"

if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: Scoop manifest rendering only accepts stable tags (vX.Y.Z): $TAG" >&2
  exit 1
fi

if [[ ! -f "$SHA256SUMS" ]]; then
  echo "error: SHA256SUMS not found: $SHA256SUMS" >&2
  exit 1
fi

VERSION="${TAG#v}"
BASE_URL="https://github.com/HeddleCo/heddle/releases/download/${TAG}"

# Scoop architecture key -> release target triple. Add the arm64 row when
# aarch64-pc-windows-msvc re-enters the build matrix (release.yml).
declare -A ARCHES=(
  ["64bit"]="x86_64-pc-windows-msvc"
)

# Look up a target's archive sha256 from the aggregate SHA256SUMS. Each
# line is "<hex>  <filename>"; we match on the exact zip filename so a
# substring collision (e.g. the .sha256 sidecar) can't be selected.
sha_for() {
  local target="$1"
  local artifact="heddle-${TAG}-${target}.zip"
  local sha
  sha="$(awk -v artifact="$artifact" '$2 == artifact { print $1 }' "$SHA256SUMS")"
  if [[ -z "$sha" ]]; then
    echo "error: no checksum found for $artifact in $SHA256SUMS" >&2
    exit 1
  fi
  printf '%s' "$sha"
}

# Build the per-architecture JSON block(s). Each archive extracts to a
# top-level directory matching the archive stem, so `bin`/`shortcuts`
# reference heddle.exe inside it.
ARCH_BLOCKS=""
for arch in "${!ARCHES[@]}"; do
  target="${ARCHES[$arch]}"
  stage="heddle-${TAG}-${target}"
  sha="$(sha_for "$target")"
  block=$(cat <<JSON
        "${arch}": {
            "url": "${BASE_URL}/${stage}.zip",
            "hash": "${sha}",
            "bin": "${stage}\\\\heddle.exe",
            "shortcuts": [
                [
                    "${stage}\\\\heddle.exe",
                    "heddle"
                ]
            ]
        }
JSON
)
  if [[ -n "$ARCH_BLOCKS" ]]; then
    ARCH_BLOCKS="${ARCH_BLOCKS},
${block}"
  else
    ARCH_BLOCKS="${block}"
  fi
done

mkdir -p "$(dirname "$OUTPUT")"

cat >"$OUTPUT" <<JSON
{
    "version": "${VERSION}",
    "description": "AI-native version control system",
    "homepage": "https://heddle.sh/",
    "license": "Apache-2.0",
    "architecture": {
${ARCH_BLOCKS}
    },
    "checkver": {
        "github": "https://github.com/HeddleCo/heddle"
    },
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "${BASE_URL%/${TAG}}/v\$version/heddle-v\$version-x86_64-pc-windows-msvc.zip",
                "bin": "heddle-v\$version-x86_64-pc-windows-msvc\\\\heddle.exe",
                "shortcuts": [
                    [
                        "heddle-v\$version-x86_64-pc-windows-msvc\\\\heddle.exe",
                        "heddle"
                    ]
                ]
            }
        },
        "hash": {
            "url": "\$url.sha256"
        }
    },
    "notes": [
        "Release archives are signed with cosign (keyless / Sigstore).",
        "Verify before trusting a binary:",
        "  cosign verify-blob --certificate-identity-regexp 'https://github.com/HeddleCo/heddle/.+' --certificate-oidc-issuer https://token.actions.githubusercontent.com --signature heddle-v${VERSION}-x86_64-pc-windows-msvc.zip.sig --certificate heddle-v${VERSION}-x86_64-pc-windows-msvc.zip.pem heddle-v${VERSION}-x86_64-pc-windows-msvc.zip",
        "The .sig and .pem are published alongside the archive on the GitHub Release."
    ]
}
JSON

echo "Rendered $OUTPUT"
