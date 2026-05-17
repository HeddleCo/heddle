// SPDX-License-Identifier: Apache-2.0
//
// On macOS 26+, FSKit modules ship as ExtensionKit extensions
// (productType `extensionkit-extension`, marked by
// `EXAppExtensionAttributes` in Info.plist). Unlike legacy System
// Extensions, ExtensionKit doesn't expose a programmatic
// activation API — `OSSystemExtensionRequest` returns
// `OSSystemExtensionErrorCodeExtensionNotFound (4)` because it
// looks for extensions in `Contents/Library/SystemExtensions/`
// while ExtensionKit puts them in `Contents/Extensions/`.
//
// The supported UX is:
//   1. Install the host app to /Applications (or run it from any
//      Launch-Services-known location).
//   2. The OS scans the embedded `.appex` and registers the
//      extension in the user's available-extensions list.
//   3. The user toggles it on in System Settings → General →
//      Login Items & Extensions → File System Extensions.
//
// This manager surfaces the workflow: it reports whether the
// app is launching from a registered location, and provides a
// deep link straight to the FSKit-extensions pane.

import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class ExtensionManager {
    enum Status {
        /// App is running from /Applications (or another LS-known
        /// location); the OS should be aware of the embedded
        /// extension.
        case registered
        /// App is running from DerivedData / a temp dir / etc. The
        /// OS won't surface the extension until the app is moved
        /// to /Applications.
        case unregistered
    }

    private(set) var status: Status = .unregistered
    private(set) var lastMessage: String = ""

    init() {
        refresh()
    }

    var statusLabel: String {
        switch status {
        case .registered:
            return "App is in a registered location — open System Settings to toggle the extension on."
        case .unregistered:
            return "App is running from a dev location. Move to /Applications so the extension is discoverable."
        }
    }

    /// Refresh the registration state. Today this just checks the
    /// running bundle's path; a more sophisticated probe would
    /// ask `pluginkit` whether our extension is known to the
    /// system. The bundle-path heuristic matches what
    /// LaunchServices uses to decide if extensions are surfaced.
    func refresh() {
        let bundleURL = Bundle.main.bundleURL.path
        if bundleURL.hasPrefix("/Applications/") {
            status = .registered
            lastMessage = ""
        } else {
            status = .unregistered
            lastMessage =
                "Detected bundle path: \(bundleURL)\n" +
                "Drag HeddleHost.app into /Applications, relaunch from there, " +
                "then click \"Open System Settings\" to enable the extension."
        }
    }

    /// Open the FSKit extensions pane directly. URL is the
    /// documented preference-pane anchor for this section.
    func openFileSystemExtensionsSettings() {
        let url = URL(
            string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension?Extensions"
        )!
        NSWorkspace.shared.open(url)
    }

    /// Open a Finder window pointing at the built app, so the
    /// user can drag it to /Applications without hunting through
    /// DerivedData.
    func revealAppInFinder() {
        let url = Bundle.main.bundleURL
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }
}
