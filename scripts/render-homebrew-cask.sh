#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/render-homebrew-cask.sh <tag> <SHA256SUMS> <output>

Renders the Heddle Homebrew cask for a stable GitHub Release.

  tag         Release tag, e.g. v0.3.0
  SHA256SUMS  Aggregate checksum file containing the macOS cask DMG line
  output      Destination cask path
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
  echo "error: Homebrew cask rendering only accepts stable tags (vX.Y.Z): $TAG" >&2
  exit 1
fi

if [[ ! -f "$SHA256SUMS" ]]; then
  echo "error: SHA256SUMS not found: $SHA256SUMS" >&2
  exit 1
fi

VERSION="${TAG#v}"
ARTIFACT="Heddle-${TAG}-macos-universal.dmg"
SHA256="$(awk -v artifact="$ARTIFACT" '$2 == artifact { print $1 }' "$SHA256SUMS")"

if [[ -z "$SHA256" ]]; then
  echo "error: no checksum found for $ARTIFACT in $SHA256SUMS" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUTPUT")"

cat >"$OUTPUT" <<CASK
cask "heddle" do
  version "$VERSION"
  sha256 "$SHA256"

  url "https://github.com/HeddleCo/heddle/releases/download/v#{version}/Heddle-v#{version}-macos-universal.dmg"
  name "Heddle"
  desc "AI-native version control system"
  homepage "https://heddle.sh"

  depends_on macos: ">= :tahoe"

  app "Heddle.app"
  binary "#{appdir}/Heddle.app/Contents/Resources/bin/heddle", target: "heddle"

  postflight do
    system_command "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
                   args: ["-f", "#{appdir}/Heddle.app"]
  end

  zap trash: [
    "~/Library/Application Support/Heddle",
    "~/Library/Preferences/sh.heddle.HeddleHost.plist",
  ]
end
CASK

echo "Rendered $OUTPUT"
