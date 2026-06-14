# HeddleHost Manual Build, Sign, Notarize, Test

This is the maintainer-run Mac recipe for tonight. CI can automate it later with
the Apple secrets tracked for #666; this document is intentionally self-contained
for a local Developer ID build.

## Requirements

- macOS 26.0 or newer with Xcode installed. The project deployment target is
  26.0 because Heddle's native FSKit path mounts use the FSKit V2 URL resource
  APIs.
- Apple Developer Program team with the `com.apple.developer.fskit.fsmodule`
  entitlement approved.
- Developer ID Application certificate in the login keychain.
- Developer ID Installer certificate in the login keychain.
- Developer ID provisioning profiles for both bundle IDs, with the FSKit
  entitlement enabled:
  - `sh.heddle.HeddleHost`
  - `sh.heddle.HeddleHost.HeddleFSModule`
- A notarytool keychain profile:

```bash
xcrun notarytool store-credentials "HeddleNotary" \
  --apple-id "you@example.com" \
  --team-id "33V6242M8S" \
  --password "app-specific-password"
```

Set these shell variables before building:

```bash
export HEDDLE_TEAM_ID="33V6242M8S"
export HEDDLE_DEVELOPER_ID="Developer ID Application: HeddleCo, LLC (33V6242M8S)"
export HEDDLE_INSTALLER_ID="Developer ID Installer: HeddleCo, LLC (33V6242M8S)"
export HEDDLE_NOTARY_PROFILE="HeddleNotary"
```

## 1. Build universal Rust artifacts

The Xcode extension links `target/release/libmount.a`; the macOS installer also
ships the `heddle` CLI. Build both as universal artifacts so the app, extension,
and command line stay version-locked.

```bash
cd /path/to/heddle

rustup target add aarch64-apple-darwin x86_64-apple-darwin

MACOSX_DEPLOYMENT_TARGET=26.0 CFLAGS="-mmacosx-version-min=26.0" \
  cargo build --release -p heddle-mount --features fskit \
  --target aarch64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=26.0 CFLAGS="-mmacosx-version-min=26.0" \
  cargo build --release -p heddle-mount --features fskit \
  --target x86_64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=26.0 CFLAGS="-mmacosx-version-min=26.0" \
  cargo build --release -p heddle-cli --bin heddle \
  --target aarch64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=26.0 CFLAGS="-mmacosx-version-min=26.0" \
  cargo build --release -p heddle-cli --bin heddle \
  --target x86_64-apple-darwin

mkdir -p target/release
lipo -create \
  target/aarch64-apple-darwin/release/libmount.a \
  target/x86_64-apple-darwin/release/libmount.a \
  -output target/release/libmount.a
lipo -create \
  target/aarch64-apple-darwin/release/heddle \
  target/x86_64-apple-darwin/release/heddle \
  -output target/release/heddle
chmod 0755 target/release/heddle

lipo -info target/release/libmount.a
lipo -info target/release/heddle
```

Expected output includes `x86_64 arm64`.

## 2. Archive HeddleHost

Open `crates/mount/swift/HeddleHost/HeddleHost.xcodeproj` once and confirm both
targets are set to manual signing with the Developer ID provisioning profiles
above. Do not remove the FSKit capability or sandbox entitlement; Xcode may
silently rewrite these settings.

Then archive from the command line:

```bash
cd crates/mount/swift/HeddleHost
rm -rf build/HeddleHost.xcarchive build/export

xcodebuild archive \
  -project HeddleHost.xcodeproj \
  -scheme HeddleHost \
  -configuration Release \
  -archivePath "$PWD/build/HeddleHost.xcarchive" \
  SKIP_INSTALL=NO \
  ARCHS="arm64 x86_64" \
  ONLY_ACTIVE_ARCH=NO \
  MACOSX_DEPLOYMENT_TARGET=26.0 \
  DEVELOPMENT_TEAM="$HEDDLE_TEAM_ID" \
  CODE_SIGN_STYLE=Manual \
  CODE_SIGN_IDENTITY="$HEDDLE_DEVELOPER_ID" \
  OTHER_CODE_SIGN_FLAGS="--timestamp --options runtime"
```

The built app is:

```bash
APP="$PWD/build/HeddleHost.xcarchive/Products/Applications/HeddleHost.app"
EXT="$APP/Contents/Extensions/HeddleFSModule.appex"
```

## 3. Verify and, if needed, re-sign

If Xcode used the correct Developer ID profiles, verification should pass:

```bash
codesign --verify --strict --verbose=2 "$EXT"
codesign --verify --strict --verbose=2 "$APP"
codesign --force --timestamp --options runtime \
  --sign "$HEDDLE_DEVELOPER_ID" "$PWD/../../../../target/release/heddle"
codesign --verify --strict --verbose=2 "$PWD/../../../../target/release/heddle"
codesign -d --entitlements :- "$APP"
codesign -d --entitlements :- "$EXT"
```

Both entitlement dumps must include:

- `com.apple.developer.fskit.fsmodule`
- `com.apple.security.app-sandbox`

If manual re-signing is needed, embed the matching provisioning profiles first,
then sign inside-out:

```bash
cp /path/to/HeddleHost.provisionprofile \
  "$APP/Contents/embedded.provisionprofile"
cp /path/to/HeddleFSModule.provisionprofile \
  "$EXT/Contents/embedded.provisionprofile"

codesign --force --timestamp --options runtime \
  --sign "$HEDDLE_DEVELOPER_ID" "$EXT"
codesign --force --timestamp --options runtime \
  --sign "$HEDDLE_DEVELOPER_ID" "$APP"

codesign --verify --deep --strict --verbose=2 "$APP"
spctl -a -vvv -t install "$APP"
```

## 4. Notarize and staple

```bash
ditto -c -k --keepParent "$APP" build/HeddleHost.zip

xcrun notarytool submit build/HeddleHost.zip \
  --keychain-profile "$HEDDLE_NOTARY_PROFILE" \
  --wait

xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl -a -vvv -t install "$APP"
```

## 5. Build the one-install package

The package is the actual Mac install unit. It places:

- `Heddle.app` in `/Applications/Heddle.app`
- `heddle` in `/usr/local/bin/heddle`

The package's `postinstall` refreshes LaunchServices so the FSKit extension is
discoverable before the first `heddle start`.

```bash
./pkg/make-pkg.sh "$APP" "$PWD/../../../../target/release/heddle" build/Heddle.pkg
pkgutil --expand build/Heddle.pkg build/Heddle.pkg.expanded
pkgutil --check-signature build/Heddle.pkg
rm -rf build/Heddle.pkg.expanded
```

For unsigned local previews, omit `HEDDLE_INSTALLER_ID`. For release, the
script signs the staged CLI with `HEDDLE_DEVELOPER_ID` and signs the package
with `HEDDLE_INSTALLER_ID` when those variables are set.

Notarize and staple the package:

```bash
xcrun notarytool submit build/Heddle.pkg \
  --keychain-profile "$HEDDLE_NOTARY_PROFILE" \
  --wait

xcrun stapler staple build/Heddle.pkg
spctl -a -vvv -t install build/Heddle.pkg
```

## 6. Build the branded DMG wrapper

The release DMG wraps the package in a branded Finder window. It does not ask
the user to drag an app manually; the `.pkg` is the single installer for both
the CLI and the macOS host app. The light background is the release default; set
`HEDDLE_DMG_APPEARANCE=dark` to generate the dark variant for review.

```bash
./dmg/make-dmg.sh build/Heddle.pkg build/Heddle.dmg

HEDDLE_DMG_APPEARANCE=dark \
  ./dmg/make-dmg.sh build/Heddle.pkg build/Heddle-dark.dmg
```

Verify the generated image before publishing:

```bash
hdiutil verify build/Heddle.dmg
codesign --force --sign "$HEDDLE_DEVELOPER_ID" build/Heddle.dmg

xcrun notarytool submit build/Heddle.dmg \
  --keychain-profile "$HEDDLE_NOTARY_PROFILE" \
  --wait

xcrun stapler staple build/Heddle.dmg
spctl -a -vvv -t open --context context:primary-signature build/Heddle.dmg
```

## 7. Local install and approval test

Install the package, then confirm both the CLI and app are present:

```bash
sudo rm -rf /Applications/Heddle.app
sudo rm -f /usr/local/bin/heddle
sudo installer -pkg build/Heddle.pkg -target /

command -v heddle
heddle --version

pluginkit -m -p com.apple.fskit.fsmodule | grep sh.heddle.HeddleHost.HeddleFSModule
open /Applications/Heddle.app
```

Open:

```text
System Settings > General > Login Items & Extensions > File System Extensions
```

Switch to the category-grouped Extensions view if the app-grouped toggle does
not move. Toggle `Heddle` on, then confirm pluginkit reports a leading `+`:

```bash
pluginkit -m -p com.apple.fskit.fsmodule | grep sh.heddle.HeddleHost.HeddleFSModule
```

Finally test the CLI path with the same repo format produced by this checkout:

```bash
cd /path/to/test/repo
heddle start fskit-smoke --workspace virtualized
```

Expected result: the command mounts the virtualized workspace through FSKit
rather than falling back to NFS.

## CI note for #666

The eventual CI version should source the Developer ID certificate, Apple team
ID, FSKit provisioning profiles, and notarytool credentials from #666's secrets.
Keep this manual recipe as the fallback path whenever the Swift/Xcode archive is
not buildable in Linux CI.
