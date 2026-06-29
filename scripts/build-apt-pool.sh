#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/build-apt-pool.sh <tag> <SHA256SUMS> <staging-dir> <output-dir>

Builds the Heddle apt pool + signed index from the Linux release archives.

  tag          Release tag, e.g. v0.3.0
  SHA256SUMS   Aggregate checksum file (used to verify the linux tarballs)
  staging-dir  Directory holding the downloaded release artifacts (the
               *-unknown-linux-gnu.tar.gz archives live somewhere under it)
  output-dir   Destination apt tree (mirrors HeddleCo/apt-heddle root):
                 pool/main/h/heddle/heddle_<ver>_<arch>.deb
                 pool/main/h/heddle/heddle-archive-keyring_<ver>_all.deb
                 dists/stable/main/binary-<arch>/Packages{,.gz}
                 dists/stable/Release   (+ Release.gpg / InRelease when signing)
                 heddle-archive-keyring.gpg   (dearmored public key, repo root)

Channel: apt. Parallel to scripts/render-scoop-manifest.sh (Scoop) and
scripts/render-homebrew-cask.sh (Homebrew) on the shared publish-manifest
substrate. Consumes the git-backed apt-heddle composition branch
(docs/design/apt-hosting-gpg-spike.md). Stable tags only.

GPG signing is opt-in via the environment, so the script runs in CI (where
the Ed25519 signing subkey is imported into an ephemeral GNUPGHOME) and
locally (unsigned dry-run for shape validation):

  HEDDLE_APT_GPG_KEY_ID    key/subkey id or fingerprint to sign Release with.
                           When set, the script writes Release.gpg + InRelease
                           and exports the dearmored public key to the repo
                           root. When unset, the index is built unsigned and a
                           warning is printed (local dry-run only — CI must set
                           it; the publish-manifests apt leg asserts this).
  GNUPGHOME                honoured as-is (ephemeral GNUPGHOME per #328
                           Decision 2); the caller imports the subkey into it.

Architectures: amd64 (x86_64-unknown-linux-gnu) + arm64
(aarch64-unknown-linux-gnu). Suite: a single rolling `stable main`
(#328 Decision 3).
USAGE
}

if [[ $# -ne 4 ]]; then
  usage
  exit 2
fi

TAG="$1"
SHA256SUMS="$2"
STAGING="$3"
OUTPUT="$4"

if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: apt pool build only accepts stable tags (vX.Y.Z): $TAG" >&2
  exit 1
fi

if [[ ! -f "$SHA256SUMS" ]]; then
  echo "error: SHA256SUMS not found: $SHA256SUMS" >&2
  exit 1
fi

if [[ ! -d "$STAGING" ]]; then
  echo "error: staging dir not found: $STAGING" >&2
  exit 1
fi

for tool in dpkg-deb ar apt-ftparchive gzip; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: required tool not found on PATH: $tool" >&2
    exit 1
  fi
done

VERSION="${TAG#v}"
MAINTAINER="Heddle <release@heddle.sh>"
HOMEPAGE="https://heddle.sh/"
DESC_SHORT="AI-native version control system"
DESC_LONG="Heddle is an AI-native version control system that overlays git."
# Keyring package version is independent of the CLI version; bump when the
# embedded trust anchor changes. Held at 1 until the first primary rotation.
KEYRING_VERSION="1"

# apt arch -> release target triple. Both linux-gnu legs are glibc-dynamic
# (glibc floor 2.35, #549), so a single rolling suite carries both.
declare -A ARCHES=(
  ["amd64"]="x86_64-unknown-linux-gnu"
  ["arm64"]="aarch64-unknown-linux-gnu"
)

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

POOLDIR="$OUTPUT/pool/main/h/heddle"
mkdir -p "$POOLDIR"

# Verify a target's archive sha256 against the aggregate SHA256SUMS. Each
# line is "<hex>  <filename>"; match the exact tarball filename so a
# substring collision (e.g. the .sha256 sidecar) can't be selected.
sha_for() {
  local target="$1"
  local artifact="heddle-${TAG}-${target}.tar.gz"
  local sha
  sha="$(awk -v artifact="$artifact" '$2 == artifact { print $1 }' "$SHA256SUMS")"
  if [[ -z "$sha" ]]; then
    echo "error: no checksum found for $artifact in $SHA256SUMS" >&2
    exit 1
  fi
  printf '%s' "$sha"
}

# Locate the staged tarball (download-artifact nests it under per-artifact
# subdirs; find the exact basename anywhere under staging).
find_archive() {
  local artifact="$1"
  local found
  found="$(find "$STAGING" -type f -name "$artifact" -print -quit)"
  if [[ -z "$found" ]]; then
    echo "error: release archive not found under $STAGING: $artifact" >&2
    exit 1
  fi
  printf '%s' "$found"
}

# Build one architecture .deb from its release tarball. The tarball extracts
# to a top-level heddle-<tag>-<target>/ dir containing the `heddle` binary
# plus README/LICENSE/NOTICE (release.yml stage step).
build_cli_deb() {
  local arch="$1" target="$2"
  local artifact="heddle-${TAG}-${target}.tar.gz"
  local archive expected_sha actual_sha
  archive="$(find_archive "$artifact")"
  expected_sha="$(sha_for "$target")"
  actual_sha="$(sha256sum "$archive" | awk '{ print $1 }')"
  if [[ "$actual_sha" != "$expected_sha" ]]; then
    echo "error: sha256 mismatch for $artifact" >&2
    echo "  expected $expected_sha (SHA256SUMS)" >&2
    echo "  actual   $actual_sha ($archive)" >&2
    exit 1
  fi

  local root="$WORK/cli-$arch"
  local stage="heddle-${TAG}-${target}"
  rm -rf "$root"
  mkdir -p "$root/usr/bin" "$root/usr/share/doc/heddle" "$root/DEBIAN"
  tar -xzf "$archive" -C "$WORK"
  install -m 0755 "$WORK/$stage/heddle" "$root/usr/bin/heddle"
  install -m 0644 "$WORK/$stage/README.md" "$root/usr/share/doc/heddle/README.md"
  install -m 0644 "$WORK/$stage/LICENSE" "$root/usr/share/doc/heddle/copyright"
  install -m 0644 "$WORK/$stage/NOTICE" "$root/usr/share/doc/heddle/NOTICE"

  cat >"$root/DEBIAN/control" <<CONTROL
Package: heddle
Version: ${VERSION}
Architecture: ${arch}
Maintainer: ${MAINTAINER}
Section: vcs
Priority: optional
Homepage: ${HOMEPAGE}
Description: ${DESC_SHORT}
 ${DESC_LONG}
CONTROL

  local deb="$POOLDIR/heddle_${VERSION}_${arch}.deb"
  # Deterministic, reproducible-ish: pin the package mtime to the source
  # epoch when CI provides one (apt diffs the pool by content).
  dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null
  echo "Built $deb"
}

# Build the heddle-archive-keyring package (arch: all). It ships the
# dearmored public key into /usr/share/keyrings so the trust anchor
# self-updates on `apt upgrade` (#328 Decision 3). When no key is configured
# (local dry-run) a placeholder marker is shipped so the package shape is
# still validated; CI always supplies the real key.
build_keyring_deb() {
  local root="$WORK/keyring"
  rm -rf "$root"
  mkdir -p "$root/usr/share/keyrings" "$root/usr/share/doc/heddle-archive-keyring" "$root/DEBIAN"

  if [[ -s "$OUTPUT/heddle-archive-keyring.gpg" ]]; then
    install -m 0644 "$OUTPUT/heddle-archive-keyring.gpg" \
      "$root/usr/share/keyrings/heddle-archive-keyring.gpg"
  else
    echo "warning: no dearmored public key available; shipping placeholder keyring (dry-run)" >&2
    printf 'PLACEHOLDER heddle-archive-keyring (unsigned dry-run build)\n' \
      >"$root/usr/share/keyrings/heddle-archive-keyring.gpg"
  fi

  cat >"$root/usr/share/doc/heddle-archive-keyring/copyright" <<'COPY'
The Heddle apt archive signing key, distributed under Apache-2.0 alongside
the heddle project. See https://heddle.sh/ for details.
COPY

  cat >"$root/DEBIAN/control" <<CONTROL
Package: heddle-archive-keyring
Version: ${KEYRING_VERSION}
Architecture: all
Maintainer: ${MAINTAINER}
Section: utils
Priority: optional
Homepage: ${HOMEPAGE}
Description: GnuPG archive key for the Heddle apt repository
 This package contains the GnuPG archive key used to verify packages in the
 Heddle apt repository at https://apt.heddle.sh. Installing it keeps the
 signing key current across future apt upgrades.
CONTROL

  local deb="$POOLDIR/heddle-archive-keyring_${KEYRING_VERSION}_all.deb"
  dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null
  echo "Built $deb"
}

# Export the dearmored public key to the repo root for manual installers.
# Done before the keyring .deb so the package embeds the same bytes.
export_public_key() {
  if [[ -z "${HEDDLE_APT_GPG_KEY_ID:-}" ]]; then
    return 0
  fi
  gpg --export "$HEDDLE_APT_GPG_KEY_ID" >"$OUTPUT/heddle-archive-keyring.gpg"
  echo "Exported dearmored public key -> $OUTPUT/heddle-archive-keyring.gpg"
}

# Generate the Packages index per arch and the suite Release file.
build_index() {
  local dists="$OUTPUT/dists/stable"
  for arch in "${!ARCHES[@]}"; do
    mkdir -p "$dists/main/binary-${arch}"
  done

  # apt-ftparchive scans the pool relative to OUTPUT so Filename: paths are
  # repo-root-relative (pool/main/...), which is what apt expects.
  for arch in "${!ARCHES[@]}"; do
    ( cd "$OUTPUT" && apt-ftparchive --arch "$arch" packages pool/main ) \
      >"$dists/main/binary-${arch}/Packages"
    gzip -9 -c "$dists/main/binary-${arch}/Packages" \
      >"$dists/main/binary-${arch}/Packages.gz"
  done

  local arch_list
  arch_list="$(printf '%s ' "${!ARCHES[@]}" | sed 's/ $//')"

  ( cd "$OUTPUT" && apt-ftparchive \
      -o "APT::FTPArchive::Release::Origin=Heddle" \
      -o "APT::FTPArchive::Release::Label=Heddle" \
      -o "APT::FTPArchive::Release::Suite=stable" \
      -o "APT::FTPArchive::Release::Codename=stable" \
      -o "APT::FTPArchive::Release::Components=main" \
      -o "APT::FTPArchive::Release::Architectures=${arch_list}" \
      release dists/stable ) >"$dists/Release"
  echo "Generated dists/stable/Release"
}

# Sign Release: detached Release.gpg + inline InRelease. Skipped (with a
# loud warning) when no key id is configured — the CI apt leg always sets it.
sign_release() {
  local dists="$OUTPUT/dists/stable"
  if [[ -z "${HEDDLE_APT_GPG_KEY_ID:-}" ]]; then
    echo "warning: HEDDLE_APT_GPG_KEY_ID unset — Release is UNSIGNED (dry-run only)" >&2
    return 0
  fi
  gpg --batch --yes --local-user "$HEDDLE_APT_GPG_KEY_ID" \
    --armor --detach-sign --output "$dists/Release.gpg" "$dists/Release"
  gpg --batch --yes --local-user "$HEDDLE_APT_GPG_KEY_ID" \
    --clearsign --output "$dists/InRelease" "$dists/Release"
  echo "Signed dists/stable/Release (Release.gpg + InRelease)"
}

mkdir -p "$OUTPUT"
export_public_key
for arch in "${!ARCHES[@]}"; do
  build_cli_deb "$arch" "${ARCHES[$arch]}"
done
build_keyring_deb
build_index
sign_release

echo "Built apt pool at $OUTPUT"
