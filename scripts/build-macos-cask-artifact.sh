#!/usr/bin/env bash
set -euo pipefail
export COPYFILE_DISABLE=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOST_DIR="$REPO_ROOT/crates/mount/swift/HeddleHost"

TAG="${HEDDLE_TAG:-}"
VERSION="${HEDDLE_VERSION:-${TAG#v}}"
OUTPUT_DIR="${HEDDLE_OUTPUT_DIR:-$REPO_ROOT/dist}"
TEAM_ID="${HEDDLE_TEAM_ID:-}"
DEVELOPER_ID="${HEDDLE_DEVELOPER_ID:-}"
HOST_PROFILE="${HEDDLE_HOST_PROVISION_PROFILE:-}"
FSMODULE_PROFILE="${HEDDLE_FSMODULE_PROVISION_PROFILE:-}"
NOTARY_KEY="${HEDDLE_NOTARY_KEY:-}"
NOTARY_KEY_ID="${HEDDLE_NOTARY_KEY_ID:-}"
NOTARY_ISSUER_ID="${HEDDLE_NOTARY_ISSUER_ID:-}"

require_env() {
  local name="$1"
  local value="$2"
  if [[ -z "$value" ]]; then
    echo "error: $name is required" >&2
    exit 1
  fi
}

require_file() {
  local name="$1"
  local path="$2"
  if [[ ! -f "$path" ]]; then
    echo "error: $name not found: $path" >&2
    exit 1
  fi
}

require_env HEDDLE_TAG "$TAG"
require_env HEDDLE_VERSION "$VERSION"
require_env HEDDLE_TEAM_ID "$TEAM_ID"
require_env HEDDLE_DEVELOPER_ID "$DEVELOPER_ID"
require_env HEDDLE_HOST_PROVISION_PROFILE "$HOST_PROFILE"
require_env HEDDLE_FSMODULE_PROVISION_PROFILE "$FSMODULE_PROFILE"
require_env HEDDLE_NOTARY_KEY "$NOTARY_KEY"
require_env HEDDLE_NOTARY_KEY_ID "$NOTARY_KEY_ID"
require_env HEDDLE_NOTARY_ISSUER_ID "$NOTARY_ISSUER_ID"
require_file HEDDLE_HOST_PROVISION_PROFILE "$HOST_PROFILE"
require_file HEDDLE_FSMODULE_PROVISION_PROFILE "$FSMODULE_PROFILE"
require_file HEDDLE_NOTARY_KEY "$NOTARY_KEY"

if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-(rc|alpha|beta)(\.?[0-9]+)?)?$ ]]; then
  echo "error: macOS cask artifacts require a release tag (vX.Y.Z or vX.Y.Z-prerelease): $TAG" >&2
  exit 1
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: macOS cask artifact must be built on a macOS runner" >&2
  exit 1
fi

ARCHIVE_PATH="$HOST_DIR/build/HeddleHost.xcarchive"
STAGED_APP="$HOST_DIR/build/Heddle.app"
EXTENSION="$STAGED_APP/Contents/Extensions/HeddleFSModule.appex"
CLI_PATH="$REPO_ROOT/target/release/heddle"
LIBMOUNT_PATH="$REPO_ROOT/target/release/libmount.a"
DMG_PATH="$OUTPUT_DIR/Heddle-${TAG}-macos-universal.dmg"
NOTARY_ZIP="$HOST_DIR/build/Heddle-${TAG}-notary.zip"

cd "$REPO_ROOT"

rustup target add aarch64-apple-darwin x86_64-apple-darwin

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  MACOSX_DEPLOYMENT_TARGET=26.0 CFLAGS="-mmacosx-version-min=26.0" \
    cargo build --release --locked -p heddle-mount --features fskit --target "$target"
  MACOSX_DEPLOYMENT_TARGET=26.0 CFLAGS="-mmacosx-version-min=26.0" \
    cargo build --release --locked -p heddle-cli --bin heddle --features mount --target "$target"
done

mkdir -p "$(dirname "$CLI_PATH")"
lipo -create \
  "$REPO_ROOT/target/aarch64-apple-darwin/release/libmount.a" \
  "$REPO_ROOT/target/x86_64-apple-darwin/release/libmount.a" \
  -output "$LIBMOUNT_PATH"
lipo -create \
  "$REPO_ROOT/target/aarch64-apple-darwin/release/heddle" \
  "$REPO_ROOT/target/x86_64-apple-darwin/release/heddle" \
  -output "$CLI_PATH"
chmod 0755 "$CLI_PATH"
lipo -info "$LIBMOUNT_PATH"
lipo -info "$CLI_PATH"

rm -rf "$HOST_DIR/build"
mkdir -p "$HOST_DIR/build" "$OUTPUT_DIR"

xcodebuild archive \
  -project "$HOST_DIR/HeddleHost.xcodeproj" \
  -scheme HeddleHost \
  -configuration Release \
  -archivePath "$ARCHIVE_PATH" \
  SKIP_INSTALL=NO \
  ARCHS="arm64 x86_64" \
  ONLY_ACTIVE_ARCH=NO \
  MACOSX_DEPLOYMENT_TARGET=26.0 \
  MARKETING_VERSION="${VERSION%%-*}" \
  DEVELOPMENT_TEAM="$TEAM_ID" \
  CODE_SIGNING_ALLOWED=NO

ditto --norsrc --noextattr --noqtn --noacl \
  "$ARCHIVE_PATH/Products/Applications/HeddleHost.app" \
  "$STAGED_APP"
mkdir -p "$STAGED_APP/Contents/Resources/bin"
ditto --norsrc --noextattr --noqtn --noacl \
  "$CLI_PATH" \
  "$STAGED_APP/Contents/Resources/bin/heddle"
chmod 0755 "$STAGED_APP/Contents/Resources/bin/heddle"

cp "$HOST_PROFILE" "$STAGED_APP/Contents/embedded.provisionprofile"
cp "$FSMODULE_PROFILE" "$EXTENSION/Contents/embedded.provisionprofile"

codesign --force --timestamp --options runtime \
  --sign "$DEVELOPER_ID" \
  "$STAGED_APP/Contents/Resources/bin/heddle"
codesign --force --timestamp --options runtime \
  --entitlements "$HOST_DIR/HeddleFSModule/HeddleFSModule.entitlements" \
  --sign "$DEVELOPER_ID" \
  "$EXTENSION"
codesign --force --timestamp --options runtime \
  --entitlements "$HOST_DIR/HeddleHost/HeddleHost.entitlements" \
  --sign "$DEVELOPER_ID" \
  "$STAGED_APP"

codesign --verify --deep --strict --verbose=2 "$STAGED_APP"

ditto -c -k --keepParent "$STAGED_APP" "$NOTARY_ZIP"
xcrun notarytool submit "$NOTARY_ZIP" \
  --key "$NOTARY_KEY" \
  --key-id "$NOTARY_KEY_ID" \
  --issuer "$NOTARY_ISSUER_ID" \
  --wait
xcrun stapler staple "$STAGED_APP"
xcrun stapler validate "$STAGED_APP"
spctl -a -vvv -t install "$STAGED_APP"

"$HOST_DIR/dmg/make-dmg.sh" "$STAGED_APP" "$DMG_PATH"
codesign --force --timestamp --sign "$DEVELOPER_ID" "$DMG_PATH"
xcrun notarytool submit "$DMG_PATH" \
  --key "$NOTARY_KEY" \
  --key-id "$NOTARY_KEY_ID" \
  --issuer "$NOTARY_ISSUER_ID" \
  --wait
xcrun stapler staple "$DMG_PATH"
hdiutil verify "$DMG_PATH"
spctl -a -vvv -t open --context context:primary-signature "$DMG_PATH"

( cd "$OUTPUT_DIR" && shasum -a 256 "$(basename "$DMG_PATH")" > "$(basename "$DMG_PATH").sha256" )
echo "Created $DMG_PATH"
