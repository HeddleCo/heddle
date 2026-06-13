#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

APP_PATH="${1:-${APP_PATH:-$HOST_DIR/build/HeddleHost.xcarchive/Products/Applications/HeddleHost.app}}"
OUTPUT_PATH="${2:-${OUTPUT_PATH:-$HOST_DIR/build/Heddle.dmg}}"
APPEARANCE="${HEDDLE_DMG_APPEARANCE:-light}"
BACKGROUND="$SCRIPT_DIR/assets/dmg-background-$APPEARANCE.png"
VOLNAME="${HEDDLE_DMG_VOLUME:-Heddle}"

if [[ ! -d "$APP_PATH" ]]; then
  echo "error: app bundle not found: $APP_PATH" >&2
  exit 1
fi

if [[ ! -f "$BACKGROUND" ]]; then
  echo "error: DMG background not found: $BACKGROUND" >&2
  echo "hint: set HEDDLE_DMG_APPEARANCE=light or HEDDLE_DMG_APPEARANCE=dark" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/heddle-dmg.XXXXXX")"
STAGE_DIR="$WORK_DIR/stage"
MOUNT_DIR="$WORK_DIR/mount"
READWRITE_DMG="$WORK_DIR/Heddle-readwrite.dmg"

cleanup() {
  if mount | grep -Fq " on $MOUNT_DIR "; then
    hdiutil detach "$MOUNT_DIR" -quiet || true
  fi
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

mkdir -p "$STAGE_DIR" "$MOUNT_DIR"
ditto "$APP_PATH" "$STAGE_DIR/Heddle.app"
ln -s /Applications "$STAGE_DIR/Applications"

hdiutil create \
  -volname "$VOLNAME" \
  -srcfolder "$STAGE_DIR" \
  -ov \
  -format UDRW \
  -fs HFS+ \
  "$READWRITE_DMG" >/dev/null

hdiutil attach "$READWRITE_DMG" \
  -mountpoint "$MOUNT_DIR" \
  -nobrowse \
  -noautoopen \
  -quiet

mkdir -p "$MOUNT_DIR/.background"
cp "$BACKGROUND" "$MOUNT_DIR/.background/background.png"

osascript >/dev/null <<APPLESCRIPT
tell application "Finder"
  tell disk "$VOLNAME"
    open
    delay 0.6
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {120, 90, 900, 570}
    set viewOptions to the icon view options of container window
    set arrangement of viewOptions to not arranged
    set icon size of viewOptions to 128
    set background picture of viewOptions to file ".background:background.png"
    set position of item "Heddle.app" of container window to {218, 265}
    set position of item "Applications" of container window to {562, 265}
    update without registering applications
    delay 0.5
    close
  end tell
end tell
APPLESCRIPT

sync
hdiutil detach "$MOUNT_DIR" -quiet
hdiutil convert "$READWRITE_DMG" \
  -format UDZO \
  -imagekey zlib-level=9 \
  -ov \
  -o "$OUTPUT_PATH" >/dev/null

echo "Created $OUTPUT_PATH"
