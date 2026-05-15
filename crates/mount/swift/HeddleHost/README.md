# HeddleHost — FSKit ExtensionKit container

This Xcode project is the macOS-only container for the Heddle
FSKit module. It exists alongside (not inside) the `heddle` CLI:
the CLI is platform-agnostic Rust, this project is the per-OS
surface required by Apple for FSKit.

On macOS 26+, FSKit modules ship as **ExtensionKit** extensions
(not the legacy System Extension model). The host app's only job
is to be a discoverable bundle in `/Applications` so LaunchServices
can register the embedded `.appex`. It has no UI: the
`LSUIElement = YES` Info.plist key suppresses the Dock icon, and
no window opens on launch unless the user explicitly opens the
app from Finder.

## Target user experience

```
$ brew install heddleco/heddle/heddle
$ heddle thread start --workspace light mybranch
   ⚠  Heddle FSKit extension not enabled.
      Opening System Settings — toggle "Heddle" on under
      File System Extensions, then re-run.
   ℹ  Using NFS fallback for this run.
[user toggles, comes back]
$ heddle thread start --workspace light mybranch
   ✓ mounted at .repo-heddle-mounts/mybranch (via FSKit)
```

Zero windows to dismiss, zero buttons to click in the host app.
The toggle in System Settings is the only user interaction macOS
requires, and we can't bypass it — Apple enforces it as a
security check for any file-system extension.

## What's here

```
HeddleHost/                  ← macOS host app target (invisible)
  HeddleHostApp.swift         App entry (SwiftUI, no Dock icon)
  ContentView.swift           Diagnostic window (only visible if user opens .app)
  ExtensionManager.swift      Status probe + Settings deeplink
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
```

## How the CLI knows whether FSKit is ready

The Rust side has a `mount::fskit::readiness::probe()` function
that shells out to `pluginkit -m -p com.apple.fskit.fsmodule` and
looks for the `sh.heddle.HeddleHost.HeddleFSModule` bundle ID.
Possible results:

| State | What CLI does |
|---|---|
| `Ready` (line starts with `+`) | Runs `mount -t heddle -o t=<thread> <repo> <mp>` via the kernel route |
| `NeedsApproval` (line starts with `-`) | Prints the setup hint, opens System Settings, falls through to NFS for this run |
| `NotInstalled` (no line for our ID) | Silent fallback to NFS (host app not in /Applications yet) |
| `Unknown` (`pluginkit` failed) | Silent fallback to NFS |

The integration lives in
[`crates/cli/src/cli/commands/mount_lifecycle.rs`](../../../../cli/src/cli/commands/mount_lifecycle.rs)
in the `macos` module. The user never has to explicitly tell the
CLI to use FSKit vs NFS — the readiness probe picks the best
available path per call.

## Build steps

### 1. Build the Rust core

```bash
cd ../../../..       # heddle repo root
cargo build --release -p heddle-mount --features fskit
```

Produces `target/release/libmount.a` (the staticlib the extension
links) and regenerates the Swift bridging header.

### 2. Build the host + extension

Open `HeddleHost.xcodeproj` in Xcode, ensure the "HeddleHost"
scheme is selected in the toolbar, and Run (or Archive for a
release build).

For headless / CI builds:
```bash
xcodebuild -project HeddleHost.xcodeproj \
  -scheme HeddleHost -configuration Release \
  CODE_SIGN_IDENTITY="Developer ID Application: …" \
  build
```

### 3. Install

Drag `HeddleHost.app` into `/Applications`. LaunchServices scans
the bundle and registers the embedded `.appex` with the system.
Force-refresh with:
```bash
lsregister -f /Applications/HeddleHost.app
# (lsregister lives at /System/Library/Frameworks/CoreServices.framework/\
#   Versions/A/Frameworks/LaunchServices.framework/Support/lsregister)
```

### 4. Approve once

`System Settings → General → Login Items & Extensions → File
System Extensions → Heddle [toggle on]`.

After this, every `heddle thread start --workspace light` will
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

The eventual `brew install heddleco/heddle/heddle` should:

1. Install `heddle` to `/opt/homebrew/bin/heddle` (or
   `/usr/local/bin` on Intel).
2. Install `HeddleHost.app` to `/Applications/HeddleHost.app`.
3. Run `lsregister -f /Applications/HeddleHost.app` in
   `post_install` so the extension is discoverable on the first
   `heddle thread start`.

A Homebrew **cask** is the right shape because it ships a `.app`:

```ruby
# Pseudo-cask in heddleco/homebrew-heddle:
cask "heddle" do
  version "0.3.0"
  sha256 "..."
  url "https://github.com/HeddleCo/heddle/releases/download/v#{version}/heddle-#{version}-macos.dmg"
  name "Heddle"
  desc "AI-native version control system"
  homepage "https://heddle.sh"

  pkg "Heddle-#{version}.pkg"

  postflight do
    system_command "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Support/lsregister",
      args: ["-f", "/Applications/HeddleHost.app"]
  end

  uninstall pkgutil: "sh.heddle.HeddleHost"
end
```

The `.pkg` payload contains both `heddle` (CLI) and
`HeddleHost.app`. Building the pkg + signing + notarizing is the
release-engineering step that depends on:

- Apple Developer Program enrollment ($99/year)
- The `com.apple.developer.fskit.fsmodule` entitlement (request
  via the Developer portal — Apple gates this)
- Developer ID Application certificate
- A notarization workflow (`xcrun notarytool` + `xcrun stapler`)

The CLI code in `mount_lifecycle.rs` is already brew-ready — it
detects the extension state and adapts. The only blocker for
shipping the cask is the signing/notarization pipeline.

## Status today

| Piece | State |
|---|---|
| Xcode project | Clean, builds host + extension |
| Host app | Invisible (`LSUIElement`), diagnostic window only |
| Extension entry point | `@main UnaryFileSystemExtension` wired |
| `FSUnaryFileSystem` ops | `probeResource` + `loadResource` working |
| `FSVolume.Operations` | Conformed — lookup, getattr, read, write, enumerate, flush dispatch into Rust |
| Rust C ABI bridge | `heddle_fskit_open_thread` connects extension → mount core |
| Readiness probe | `pluginkit`-based, wired into `mount_lifecycle.rs` |
| NFS fallback | Always-available, picks up when FSKit isn't ready |
| End-to-end mount + read | Working on macOS 26.4 (`mount -t heddle … && cat <mp>/file` succeeds) |
| `mtime` on returned items | Wired through the C ABI; shows mount bootstrap time in `ls -l` |
| `-o t=<thread>` option parsing | Parsed from `FSTaskOptions.taskOptions` in `loadResource`; defaults to `"main"` |
| Entitlement request | Not yet filed |
| Homebrew cask | Not yet published |
| Code signing + notarization | Not yet set up |
