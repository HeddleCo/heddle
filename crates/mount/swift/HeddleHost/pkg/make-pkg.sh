#!/usr/bin/env bash
set -euo pipefail
export COPYFILE_DISABLE=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$HOST_DIR/../../../.." && pwd)"

APP_PATH="${1:-${APP_PATH:-$HOST_DIR/build/HeddleHost.xcarchive/Products/Applications/HeddleHost.app}}"
CLI_PATH="${2:-${CLI_PATH:-$REPO_ROOT/target/release/heddle}}"
FSMONITOR_WORKER_PATH="${FSMONITOR_WORKER_PATH:-$(dirname "$CLI_PATH")/heddle-fsmonitor-worker}"
OUTPUT_PATH="${3:-${OUTPUT_PATH:-$HOST_DIR/build/Heddle.pkg}}"
VERSION="${HEDDLE_VERSION:-$(awk -F'"' '/^\[package\]/{in_pkg=1; next} /^\[/{in_pkg=0} in_pkg && /^version[[:space:]]*=/{print $2; exit}' "$REPO_ROOT/crates/cli/Cargo.toml")}"
IDENTIFIER="${HEDDLE_PKG_IDENTIFIER:-sh.heddle.Heddle}"
INSTALLER_ID="${HEDDLE_INSTALLER_ID:-}"
APPLICATION_ID="${HEDDLE_DEVELOPER_ID:-}"

if [[ ! -d "$APP_PATH" ]]; then
  echo "error: app bundle not found: $APP_PATH" >&2
  exit 1
fi

if [[ ! -f "$CLI_PATH" ]]; then
  echo "error: heddle CLI binary not found: $CLI_PATH" >&2
  echo "hint: build it with: cargo build --release -p heddle-cli --bin heddle" >&2
  exit 1
fi

if [[ ! -x "$CLI_PATH" ]]; then
  echo "error: heddle CLI binary is not executable: $CLI_PATH" >&2
  exit 1
fi

if [[ ! -x "$FSMONITOR_WORKER_PATH" ]]; then
  echo "error: heddle fsmonitor worker not found or not executable: $FSMONITOR_WORKER_PATH" >&2
  echo "hint: build it with: cargo build --release -p heddle-cli --bin heddle-fsmonitor-worker" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/heddle-pkg.XXXXXX")"
PKGROOT="$WORK_DIR/root"
PKGSCRIPTS="$WORK_DIR/scripts"
TEMP_PKG="$WORK_DIR/Heddle.pkg"

cleanup() {
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

mkdir -p "$PKGROOT/Applications" "$PKGROOT/usr/local/bin" "$PKGSCRIPTS"
ditto --norsrc --noextattr --noqtn --noacl "$APP_PATH" "$PKGROOT/Applications/Heddle.app"
ditto --norsrc --noextattr --noqtn --noacl "$CLI_PATH" "$PKGROOT/usr/local/bin/heddle"
ditto --norsrc --noextattr --noqtn --noacl "$FSMONITOR_WORKER_PATH" "$PKGROOT/usr/local/bin/heddle-fsmonitor-worker"
ditto --norsrc --noextattr --noqtn --noacl "$SCRIPT_DIR/scripts/postinstall" "$PKGSCRIPTS/postinstall"
chmod 0755 "$PKGROOT/usr/local/bin/heddle" "$PKGROOT/usr/local/bin/heddle-fsmonitor-worker" "$PKGSCRIPTS/postinstall"
/usr/bin/xattr -cr "$PKGROOT" "$PKGSCRIPTS" 2>/dev/null || true

if [[ -n "$APPLICATION_ID" ]]; then
  codesign --force --timestamp --options runtime \
    --sign "$APPLICATION_ID" \
    "$PKGROOT/usr/local/bin/heddle" >/dev/null
  codesign --force --timestamp --options runtime \
    --sign "$APPLICATION_ID" \
    "$PKGROOT/usr/local/bin/heddle-fsmonitor-worker" >/dev/null
fi

PKGBUILD_ARGS=(
  --root "$PKGROOT"
  --scripts "$PKGSCRIPTS"
  --identifier "$IDENTIFIER"
  --version "$VERSION"
  --install-location /
  --filter '(^|/)\.DS_Store$'
  --filter '(^|/)\.svn($|/)'
  --filter '(^|/)CVS($|/)'
  --filter '(^|/)\._'
)

if [[ -n "$INSTALLER_ID" ]]; then
  PKGBUILD_ARGS+=(--sign "$INSTALLER_ID")
fi

pkgbuild "${PKGBUILD_ARGS[@]}" "$TEMP_PKG" >/dev/null
mv "$TEMP_PKG" "$OUTPUT_PATH"

echo "Created $OUTPUT_PATH"
