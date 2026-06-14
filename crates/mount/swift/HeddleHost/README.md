# HeddleHost — FSKit ExtensionKit container

This Xcode project is the macOS-only container for the Heddle
FSKit module. It exists alongside (not inside) the `heddle` CLI:
the CLI is platform-agnostic Rust, this project is the per-OS
surface required by Apple for FSKit.

On macOS 26.0+, Heddle's path-backed FSKit module ships as **ExtensionKit** extensions
(not the legacy System Extension model). The host app's only job
is to be a discoverable bundle in `/Applications` so LaunchServices
can register the embedded `.appex`. The app remains quiet by default: the
`LSUIElement = YES` Info.plist key suppresses the Dock icon, and
no window opens on launch unless the user explicitly opens the
app from Finder or the CLI deep-links to it after FSKit needs approval.

## Target user experience

```
$ brew install --cask heddleco/heddle/heddle
$ heddle start mybranch --workspace virtualized
   ⚠  Heddle FSKit extension not enabled.
      Opening System Settings — toggle "Heddle" on under
      File System Extensions.
[user toggles while the command is still running]
   ✓ mounted at .repo-heddle-mounts/mybranch (via FSKit)
```

Zero windows to dismiss in the normal CLI path. If the host app is
opened directly, it shows an onboarding card with the live pluginkit
state, a System Settings button, and an annotated SwiftUI mock of the
File System Extensions toggle. The toggle in System Settings is the
only user interaction macOS requires, and we can't bypass it — Apple
enforces it as a security check for any file-system extension.

The Mac installer is one package, not two manual installs. The release `.pkg`
places `heddle` in `/usr/local/bin/heddle`, places `Heddle.app` in
`/Applications/Heddle.app`, and refreshes LaunchServices so the embedded FSKit
module is discoverable before the first `heddle start`.

## What's here

```
HeddleHost/                  ← macOS host app target (invisible)
  HeddleHostApp.swift         App entry (SwiftUI, no Dock icon)
  ContentView.swift           Onboarding window (only visible if user opens .app)
  ExtensionManager.swift      pluginkit status probe + Settings deeplink
  HeddleHost.entitlements     Sandbox only — no programmatic activation needed
  Assets.xcassets/            Stock icon + accent color
HeddleFSModule/              ← ExtensionKit extension target (.appex)
  HeddleFSModule.swift         @main UnaryFileSystemExtension
  HeddleFSModuleFileSystem.swift   FSUnaryFileSystem subclass
  HeddleVolume.swift           FSVolume + Operations conformance
  HeddleItem.swift             FSItem subclass
  HeddleSession.swift          Rust C-ABI bridge
  HeddleFSKit.swift            Symlink to canonical C-ABI Swift glue
  Info.plist                   EXAppExtensionAttributes (fskit.fsmodule)
  HeddleFSModule.entitlements  fskit.fsmodule + sandbox
HeddleHost.xcodeproj/        ← Xcode project (synced folder groups)
  xcshareddata/xcschemes/     ← Shared scheme: Run = HeddleHost.app
README.md                    ← this file
BUILD.md                     ← Manual Mac build/sign/notarize/test recipe
```

## Compatibility and build shape

- Host app deployment target: macOS 26.0.
- HeddleFSModule deployment target: macOS 26.0.
- Build SDK: macOS 26.0 or newer; FSKit V2 URL resources are required for native path-backed mounts.
- Archive architecture: universal `arm64` + `x86_64`.
- Macs below macOS 26.0 stay on the CLI's NFS fallback and get a clear notice instead of a broken FSKit prompt.
- The user-facing Settings path is the compatibility contract:
  `System Settings > General > Login Items & Extensions > File System Extensions`.
  The `x-apple.systempreferences:` anchors are best-effort conveniences and
  fall back to the parent Login Items & Extensions pane if Apple changes them.

## How the CLI knows whether FSKit is ready

The Rust side has a `mount::fskit::readiness::probe()` function
that shells out to `pluginkit -m -p com.apple.fskit.fsmodule` and
looks for the `sh.heddle.HeddleHost.HeddleFSModule` bundle ID.
Possible results:

| State | What CLI does |
|---|---|
| `Ready` (line starts with `+`) | Runs `mount -t heddle -o t=<thread> <repo> <mp>` via the kernel route |
| `NeedsApproval` (line starts with `-`) | Prints a setup block with `System Settings → General → Login Items & Extensions → File System Extensions → enable 'Heddle'`, opens System Settings with a version-aware deep link, polls readiness for about 60 seconds, then mounts via FSKit as soon as the probe reports `Ready`; if the timer elapses, falls back to NFS for this run |
| `NotInstalled` (no line for our ID) | Prints a one-line host-app install hint, then falls back to NFS |
| `UnsupportedMacOS` (macOS < 26.0) | Prints an older-macOS notice, then falls back to NFS because URL-backed FSKit resources are unavailable |
| `Unknown` (`pluginkit` failed) | Silent fallback to NFS |

The host app's `ExtensionManager` now uses the same pluginkit signal and polls
about every two seconds while the onboarding window is open:

| Host state | Source |
|---|---|
| `FSKit enabled` | line for `sh.heddle.HeddleHost.HeddleFSModule` starts with `+` |
| `Toggle required` | line for the bundle ID starts with `-` |
| `Extension not registered` | no line for the bundle ID, app appears installed |
| `Move to /Applications` | no line and this copy is running from a dev location |

The integration lives in
[`crates/cli/src/cli/commands/mount_lifecycle.rs`](../../../../cli/src/cli/commands/mount_lifecycle.rs)
in the `macos` module. The user never has to explicitly tell the
CLI to use FSKit vs NFS — the readiness probe picks the best
available path per call.

## Build steps

### 1. Build the Rust core

```bash
cd ../../../..       # heddle repo root
MACOSX_DEPLOYMENT_TARGET=26.0 CFLAGS="-mmacosx-version-min=26.0" \
  cargo build --release -p heddle-mount --features fskit,nfs
```

Produces `target/release/libmount.a` (the staticlib the extension
links) and regenerates the Swift bridging header.

### 2. Build the host + extension

Open `HeddleHost.xcodeproj` in Xcode, ensure the "HeddleHost"
scheme is selected in the toolbar, and Run (or Archive for a
release build).

For the Developer ID archive, signing, notarization, and local smoke test,
follow [`BUILD.md`](BUILD.md). The release archive is universal
`arm64` + `x86_64` and keeps both app and extension deployment targets at
macOS 26.0.

### 3. Package or install

The release path is package-first:

```bash
./pkg/make-pkg.sh "$APP" "$REPO_ROOT/target/release/heddle" build/Heddle.pkg
./dmg/make-dmg.sh build/Heddle.pkg build/Heddle.dmg
```

`Heddle.pkg` installs both the CLI and `/Applications/Heddle.app`; its
`postinstall` runs `lsregister -f /Applications/Heddle.app`. The DMG is only a
branded wrapper around that package for website downloads.

For local app-only testing, `./dmg/make-dmg.sh "$APP" build/Heddle-app.dmg`
still creates the old drag-to-Applications window, but that is no longer the
end-user distribution shape.

### 4. Approve once

`System Settings → General → Login Items & Extensions → File
System Extensions → Heddle [toggle on]`.

After this, every `heddle start <name> --workspace virtualized` will
use FSKit transparently.

## Implementation gotchas (macOS 26.4)

Things that cost hours to figure out and aren't obvious from Apple's
docs. If you're porting another FSKit module, read this first.

### 1. `self.containerStatus = .ready` is mandatory in `loadResource`

`FSUnaryFileSystem` containers start in `.notReady`. If you call the
`replyHandler` without transitioning to `.ready`, the kernel reports
`mount: unexpected container state` and the mount fails with EAGAIN.

```swift
func loadResource(resource:options:replyHandler:) {
    // … build the FSVolume …
    self.containerStatus = FSContainerStatus.ready   // ← required
    replyHandler(volume, nil)
}
```

Per Apple's contract: `loadResource → .ready`, then volume
activation transitions to `.active`. The `.active` step happens
inside FSKit; you only need to set `.ready` yourself.

### 2. `probeResource` containerID UUID **must equal** `loadResource` volumeID UUID

The kernel's unary-FS contract requires the container ID returned
from `probeResource` to match the volume ID returned from
`loadResource`. Mismatch → mount fails with an obscure error.

Derive both from the resource path so they're stable:
```swift
static func stableUUID(for resource: FSResource) -> UUID {
    let key = "heddle://path/\((resource as? FSPathURLResource)?.url.path ?? "unknown")"
    let digest = SHA256.hash(data: Data(key.utf8))
    let bytes = Array(digest.prefix(16))
    return UUID(uuid: (bytes[0], …, bytes[15]))
}
```

### 3. `FSRequiresSecurityScopedPathURLResources = false`

Setting this to `true` in `Info.plist` breaks `mount -t heddle`
because mount(8) doesn't go through the security-scoped URL path.
Leave it `false` and call `startAccessingSecurityScopedResource()`
on the URL inside `loadResource` if you need broader sandbox grants.

### 4. Path-access entitlements: single string, not array

On macOS 26.4, the array form of
`com.apple.security.temporary-exception.files.absolute-path.read-write`
is silently ignored. The single-string form works:

```xml
<key>com.apple.security.temporary-exception.files.absolute-path.read-write</key>
<string>/</string>
```

Cross-referenced with Apple Developer Forums thread 808246. This is
the only known reliable shape for granting an FSKit module
read/write across arbitrary paths on 26.4.

### 5. **Both** host and extension need `com.apple.developer.fskit.fsmodule`

It's not enough to put the FSKit capability on the extension target
— the host bundle that contains the `.appex` needs it too. If only
the extension has it, `pluginkit` lists the module but the kernel
refuses to load it. Set it on both `.entitlements` files and ensure
both provisioning profiles include the FSKit capability.

### 6. App Sandbox is forced on by the FSKit capability

You cannot disable `com.apple.security.app-sandbox` once the FSKit
capability is enabled; Xcode silently re-adds it on every build.
Plan path access through the temporary-exception entitlement (above)
rather than fighting the sandbox.

### 7. Settings UI bug: app-grouped view doesn't toggle on 26.4

In `System Settings → General → Login Items & Extensions → File
System Extensions`, the **app-grouped** view (default) shows the
toggle but tapping it does nothing — the module stays disabled.
Switching to the **category-grouped** view (the picker at the top
of the panel) makes the toggle work. Tell users this in the setup
hint.

If even that fails, the toggle state lives in:
```
~/Library/Preferences/com.apple.fskit.enabledModules.plist
```
A direct `/usr/libexec/PlistBuddy` edit is the last-resort
workaround.

### 8. Version-mismatch caveat for repositories

A `.heddle` repo created by an older `heddle` binary may fail to
load through the FSKit extension because the on-disk format
(thread metadata, ref layout) evolved. The extension links against
the Rust core from whichever `heddle-mount` static lib it was built
with — older repos may report `UnknownThread("main")`. Always
create the test repo with the same `heddle` build that produced
the `.appex`.

### 9. ExtensionKit, not legacy System Extensions

On macOS 26+, `OSSystemExtensionRequest.activationRequest(...)`
returns `OSSystemExtensionErrorDomain Code=4` — that's the legacy
API, and FSKit modules now ship as ExtensionKit `.appex`s
discovered via LaunchServices. There is no programmatic activation;
the user toggles the extension in System Settings. The host app's
`ExtensionManager` only deeplinks Settings; it doesn't call
`OSSystemExtensionRequest`.

### 10. `.xcarchive` outside `/Applications` is rejected

`pluginkit` rejects an `.appex` whose containing `.app` lives
anywhere except `/Applications` or a SIP-protected location, with:
> plug-ins outside containing apps must be protected by SIP

If you're testing, copy the built app to `/Applications`. Don't
leave a stale `.xcarchive` next to it — `lsregister` will pick the
wrong one.

## Homebrew distribution (the target)

The eventual `brew install --cask heddleco/heddle/heddle` should:

1. Install the host app to `/Applications/Heddle.app`.
2. Link the bundled CLI to Homebrew's `bin` directory as `heddle`.
3. Run `lsregister -f /Applications/Heddle.app` in
   `post_install` so the extension is discoverable on the first
   `heddle start`.

A Homebrew **cask** is the right shape because it ships a `.app`:

```ruby
# Pseudo-cask in heddleco/homebrew-heddle:
cask "heddle" do
  version "0.3.0"
  sha256 "..."
  url "https://github.com/HeddleCo/heddle/releases/download/v#{version}/Heddle-v#{version}-macos-universal.dmg"
  name "Heddle"
  desc "AI-native version control system"
  homepage "https://heddle.sh"

  depends_on macos: ">= :tahoe"

  app "Heddle.app"
  binary "#{appdir}/Heddle.app/Contents/Resources/bin/heddle", target: "heddle"
end
```

The DMG contains `Heddle.app`; the app bundle contains the CLI at
`Contents/Resources/bin/heddle`. Homebrew handles moving the app into
`/Applications` and linking the CLI. Building, signing, and notarizing the app
DMG depends on:

- Apple Developer Program enrollment ($99/year)
- The `com.apple.developer.fskit.fsmodule` entitlement (request
  via the Developer portal — Apple gates this)
- Developer ID Application certificate
- A notarization workflow (`xcrun notarytool` + `xcrun stapler`)

The CLI code in `mount_lifecycle.rs` is already brew-ready — it
detects the extension state and adapts. The only blocker for
shipping the cask is the GitHub Actions signing/notarization secrets and the
first stable release that publishes `Heddle-v<version>-macos-universal.dmg`.

## Status today

| Piece | State |
|---|---|
| Xcode project | Clean, builds host + extension |
| Host app | Invisible (`LSUIElement`), onboarding window only |
| Extension entry point | `@main UnaryFileSystemExtension` wired |
| `FSUnaryFileSystem` ops | `probeResource` + `loadResource` working |
| `FSVolume.Operations` | Conformed — lookup, getattr, read, write, enumerate, flush dispatch into Rust |
| Rust C ABI bridge | `heddle_fskit_open_thread` connects extension → mount core |
| Readiness probe | `pluginkit`-based, wired into `mount_lifecycle.rs` and host onboarding |
| NFS fallback | Always-available, picks up when FSKit isn't ready |
| End-to-end mount + read | Working on macOS 26.4 (`mount -t heddle … && cat <mp>/file` succeeds) |
| `mtime` on returned items | Wired through the C ABI; shows mount bootstrap time in `ls -l` |
| `-o t=<thread>` option parsing | Parsed from `FSTaskOptions.taskOptions` in `loadResource`; defaults to `"main"` |
| Entitlement request | Not yet filed |
| macOS package | `pkg/make-pkg.sh` builds CLI + app payload |
| Branded DMG | `dmg/make-dmg.sh` wraps the package by default |
| Homebrew cask | Not yet published |
| Code signing + notarization | Not yet set up |
