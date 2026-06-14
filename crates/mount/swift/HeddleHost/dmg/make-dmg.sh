#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

INPUT_PATH="${1:-${INPUT_PATH:-$HOST_DIR/build/Heddle.pkg}}"
OUTPUT_PATH="${2:-${OUTPUT_PATH:-$HOST_DIR/build/Heddle.dmg}}"
APPEARANCE="${HEDDLE_DMG_APPEARANCE:-light}"
VOLNAME="${HEDDLE_DMG_VOLUME:-Heddle}"
DMG_FORMAT="${HEDDLE_DMG_FORMAT:-UDZO}"
ZLIB_LEVEL="${HEDDLE_DMG_ZLIB_LEVEL:-9}"

if [[ -d "$INPUT_PATH" && "$INPUT_PATH" == *.app ]]; then
  LAYOUT="app"
  STAGED_ITEM="Heddle.app"
  BACKGROUND_STEM="dmg-background"
elif [[ -f "$INPUT_PATH" && "$INPUT_PATH" == *.pkg ]]; then
  LAYOUT="installer"
  STAGED_ITEM="Install Heddle.pkg"
  BACKGROUND_STEM="dmg-background-installer"
else
  echo "error: input must be a .pkg installer or .app bundle: $INPUT_PATH" >&2
  exit 1
fi

BACKGROUND_SVG="$SCRIPT_DIR/assets/$BACKGROUND_STEM-$APPEARANCE.svg"
BACKGROUND_PNG="$SCRIPT_DIR/assets/$BACKGROUND_STEM-$APPEARANCE.png"

if [[ ! -f "$BACKGROUND_SVG" && ! -f "$BACKGROUND_PNG" ]]; then
  echo "error: DMG background not found: $BACKGROUND_SVG or $BACKGROUND_PNG" >&2
  echo "hint: set HEDDLE_DMG_APPEARANCE=light or HEDDLE_DMG_APPEARANCE=dark" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/heddle-dmg.XXXXXX")"
STAGE_DIR="$WORK_DIR/stage"
MOUNT_DIR="$WORK_DIR/mount"
READWRITE_DMG="$WORK_DIR/Heddle-readwrite.dmg"

cleanup() {
  hdiutil detach "$MOUNT_DIR" -quiet || true
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

mkdir -p "$STAGE_DIR" "$MOUNT_DIR"
if [[ "$LAYOUT" == "app" ]]; then
  ditto "$INPUT_PATH" "$STAGE_DIR/$STAGED_ITEM"
  ln -s /Applications "$STAGE_DIR/Applications"
else
  cp "$INPUT_PATH" "$STAGE_DIR/$STAGED_ITEM"
fi

hdiutil create \
  -volname "$VOLNAME" \
  -srcfolder "$STAGE_DIR" \
  -ov \
  -format UDRW \
  -fs HFS+ \
  "$READWRITE_DMG" >/dev/null

hdiutil attach "$READWRITE_DMG" \
  -mountpoint "$MOUNT_DIR" \
  -noautoopen \
  -quiet

mkdir -p "$MOUNT_DIR/.background"
if [[ -f "$BACKGROUND_SVG" && -x "$(command -v rsvg-convert || true)" ]]; then
  rsvg-convert \
    -f pdf \
    --dpi-x 72 \
    --dpi-y 72 \
    -o "$MOUNT_DIR/.background/background.pdf" \
    "$BACKGROUND_SVG"
  BACKGROUND_IMAGE="$MOUNT_DIR/.background/background.pdf"
elif [[ -f "$BACKGROUND_PNG" ]]; then
  cp "$BACKGROUND_PNG" "$MOUNT_DIR/.background/background.png"
  BACKGROUND_IMAGE="$MOUNT_DIR/.background/background.png"
else
  echo "error: rsvg-convert is required to render $BACKGROUND_SVG" >&2
  exit 1
fi

osascript >/dev/null <<APPLESCRIPT
tell application "Finder"
  set mountedFolder to POSIX file "$MOUNT_DIR" as alias
  set backgroundImage to POSIX file "$BACKGROUND_IMAGE" as alias
  open mountedFolder
  delay 0.6
  set containerWindow to container window of mountedFolder
  set current view of containerWindow to icon view
  set toolbar visible of containerWindow to false
  set statusbar visible of containerWindow to false
  set the bounds of containerWindow to {120, 90, 900, 570}
  set viewOptions to the icon view options of containerWindow
  set arrangement of viewOptions to not arranged
  set icon size of viewOptions to 128
  set text size of viewOptions to 10
  set background picture of viewOptions to backgroundImage
  if "$LAYOUT" is "app" then
    set position of item "Heddle.app" of mountedFolder to {218, 265}
    set position of item "Applications" of mountedFolder to {562, 265}
  else
    set position of item "Install Heddle.pkg" of mountedFolder to {390, 278}
  end if
  update mountedFolder without registering applications
  delay 0.5
  close containerWindow
end tell
APPLESCRIPT

sync
hdiutil detach "$MOUNT_DIR" -quiet
CONVERT_ARGS=(
  -format "$DMG_FORMAT"
  -ov
  -o "$OUTPUT_PATH"
)

if [[ "$DMG_FORMAT" == "UDZO" ]]; then
  CONVERT_ARGS=(-imagekey "zlib-level=$ZLIB_LEVEL" "${CONVERT_ARGS[@]}")
fi

hdiutil convert "$READWRITE_DMG" "${CONVERT_ARGS[@]}" >/dev/null

echo "Created $OUTPUT_PATH"
