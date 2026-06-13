// SPDX-License-Identifier: Apache-2.0
//
// On macOS 15.4+, FSKit modules ship as ExtensionKit extensions
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
// This manager surfaces the workflow: it asks pluginkit for the
// real FSKit module state, keeps polling while the onboarding
// window is visible, and provides a deep link to the best-known
// Settings pane for the current macOS release.

import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class ExtensionManager {
    nonisolated static let bundleIdentifier = "sh.heddle.HeddleHost.HeddleFSModule"
    nonisolated static let settingsPath =
        "System Settings > General > Login Items & Extensions > File System Extensions"

    enum Status {
        /// pluginkit lists the FSKit module with a leading "+".
        case registeredEnabled
        /// pluginkit lists the FSKit module with a leading "-".
        case registeredDisabled
        /// pluginkit did not list the module, even though the app
        /// appears to be installed in a LaunchServices-scanned
        /// location.
        case unregistered
        /// App is running from DerivedData / a temp dir / etc. The
        /// OS won't surface the extension until the app is moved to
        /// /Applications and LaunchServices scans it.
        case devLocation(String)
    }

    private(set) var status: Status = .unregistered
    private(set) var lastMessage: String = ""
    private(set) var lastRefreshed: Date?

    private var isRefreshing = false
    private var pollTimer: Timer?

    init() {
        refresh()
        pollTimer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.refresh()
            }
        }
    }

    deinit {
        pollTimer?.invalidate()
    }

    var statusLabel: String {
        switch status {
        case .registeredEnabled:
            return "FSKit enabled"
        case .registeredDisabled:
            return "Toggle required"
        case .unregistered:
            return "Extension not registered"
        case .devLocation:
            return "Move to /Applications"
        }
    }

    var statusDetail: String {
        switch status {
        case .registeredEnabled:
            return "Heddle is enabled in File System Extensions. Re-run `heddle start --workspace virtualized` to mount through FSKit."
        case .registeredDisabled:
            return "Heddle is installed but macOS has it turned off. Toggle the Heddle row on in File System Extensions."
        case .unregistered:
            return "macOS has not registered the bundled FSKit module yet. Confirm HeddleHost.app is in /Applications, then refresh LaunchServices."
        case .devLocation(let path):
            return "This copy is running from \(path). Move HeddleHost.app to /Applications so macOS can register the embedded extension."
        }
    }

    var canRevealApp: Bool {
        if case .devLocation = status {
            return true
        }
        return false
    }

    /// Refresh the registration state using the same pluginkit
    /// signal the CLI uses: "+" means enabled, "-" means installed
    /// but disabled, and absence means the module is not registered.
    func refresh() {
        guard !isRefreshing else {
            return
        }

        isRefreshing = true
        let bundlePath = Bundle.main.bundleURL.path

        Task {
            let probe = await Self.runPluginKitProbe()
            apply(probe: probe, bundlePath: bundlePath)
            isRefreshing = false
        }
    }

    /// Open the best-known Settings URL for File System Extensions.
    ///
    /// Apple documents the visible path, not a stable public URL
    /// contract. Keep the user-facing path in the UI and treat these
    /// anchors as conveniences: if a future macOS changes the anchor,
    /// the fallback still lands in Login Items & Extensions.
    func openFileSystemExtensionsSettings() {
        guard let url = settingsDestination().url else {
            openSystemSettingsApp()
            return
        }

        NSWorkspace.shared.open(url)
    }

    /// Open a Finder window pointing at the built app, so the
    /// user can drag it to /Applications without hunting through
    /// DerivedData.
    func revealAppInFinder() {
        let url = Bundle.main.bundleURL
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }

    private func apply(probe: PluginKitProbe, bundlePath: String) {
        lastRefreshed = Date()

        switch probe {
        case .enabled:
            status = .registeredEnabled
            lastMessage = ""
        case .disabled:
            status = .registeredDisabled
            lastMessage = "pluginkit lists \(Self.bundleIdentifier) with a disabled marker (-)."
        case .absent:
            if isLikelyDevLocation(bundlePath) {
                status = .devLocation(bundlePath)
                lastMessage =
                    "Drag HeddleHost.app into /Applications, relaunch it from there, " +
                    "then open System Settings."
            } else {
                status = .unregistered
                lastMessage =
                    "pluginkit did not list \(Self.bundleIdentifier). Run lsregister for /Applications/HeddleHost.app if the app was just copied."
            }
        case .failed(let message):
            if isLikelyDevLocation(bundlePath) {
                status = .devLocation(bundlePath)
            } else {
                status = .unregistered
            }
            lastMessage = "Could not run pluginkit probe: \(message)"
        }
    }

    private func settingsDestination() -> SettingsDestination {
        let version = ProcessInfo.processInfo.operatingSystemVersion

        if #available(macOS 26, *) {
            // macOS 26 Tahoe: the category-grouped Extensions tab is the
            // reliable user path for toggling FSKit modules. The anchor is
            // best-effort and may still land on the parent pane.
            return SettingsDestination(
                url: URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension?Extensions")
            )
        }

        switch version.majorVersion {
        case 15:
            // macOS 15 Sequoia places System/File System Extensions under
            // General > Login Items & Extensions.
            return SettingsDestination(
                url: URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension?Extensions")
            )
        default:
            return SettingsDestination(
                url: URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")
            )
        }
    }

    private func openSystemSettingsApp() {
        guard let appURL = NSWorkspace.shared.urlForApplication(
            withBundleIdentifier: "com.apple.systempreferences"
        ) else {
            return
        }

        NSWorkspace.shared.open(appURL)
    }

    private func isLikelyDevLocation(_ bundlePath: String) -> Bool {
        if bundlePath.hasPrefix("/Applications/") {
            return false
        }

        if bundlePath.hasPrefix("/System/Applications/") {
            return false
        }

        return true
    }

    private nonisolated static func runPluginKitProbe() async -> PluginKitProbe {
        await Task.detached(priority: .utility) {
            let process = Process()
            let outputPipe = Pipe()

            process.executableURL = URL(fileURLWithPath: "/usr/bin/pluginkit")
            process.arguments = ["-m", "-p", "com.apple.fskit.fsmodule"]
            process.standardOutput = outputPipe
            process.standardError = outputPipe

            do {
                try process.run()
            } catch {
                return .failed(error.localizedDescription)
            }

            process.waitUntilExit()

            let outputData = outputPipe.fileHandleForReading.readDataToEndOfFile()
            let output = String(data: outputData, encoding: .utf8) ?? ""

            guard process.terminationStatus == 0 else {
                let message = output.trimmingCharacters(in: .whitespacesAndNewlines)
                if message.isEmpty {
                    return .failed("pluginkit exited with status \(process.terminationStatus)")
                }
                return .failed(message)
            }

            return parsePluginKitOutput(output)
        }.value
    }

    private nonisolated static func parsePluginKitOutput(_ output: String) -> PluginKitProbe {
        for line in output.components(separatedBy: .newlines) {
            guard line.contains(bundleIdentifier) else {
                continue
            }

            let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
            guard let marker = trimmed.first else {
                continue
            }

            switch marker {
            case "+":
                return .enabled
            case "-":
                return .disabled
            default:
                return .absent
            }
        }

        return .absent
    }
}

private struct SettingsDestination {
    let url: URL?
}

private enum PluginKitProbe: Sendable {
    case enabled
    case disabled
    case absent
    case failed(String)
}
