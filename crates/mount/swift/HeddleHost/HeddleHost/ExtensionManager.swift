// SPDX-License-Identifier: Apache-2.0
//
// On macOS 26.0+, Heddle's path-backed FSKit module ships as an
// ExtensionKit extension
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
//      Login Items & Extensions → File System Extensions, with the
//      sheet grouped by Category. The per-app ("By App") grouping has
//      an Apple-side bug where the FSKit toggle silently no-ops, so we
//      always steer users to the Category view.
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
    nonisolated static let installAppName = "Heddle.app"
    nonisolated static let settingsPath =
        "System Settings > General > Login Items & Extensions > File System Extensions (group by Category)"

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

    enum InstallState: Equatable {
        case idle
        case copying
        case preparingShortcut
        case shortcutReady(String)
        case installed(String)
        case failed(String)
    }

    private(set) var status: Status = .unregistered
    private(set) var installState: InstallState = .idle
    private(set) var lastMessage: String = ""
    private(set) var lastRefreshed: Date?

    private var isRefreshing = false
    private var pollTimer: Timer?

    init() {
        refresh()
        pollTimer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.refresh()
            }
        }
    }

    deinit {
        MainActor.assumeIsolated {
            pollTimer?.invalidate()
        }
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
            return "Heddle is installed but macOS has it turned off. Open File System Extensions, switch the grouping to Category, and toggle the Heddle row on. Don't use the By App view — its FSKit toggle is a known macOS bug that silently does nothing."
        case .unregistered:
            return "macOS has not registered the bundled FSKit module yet. Confirm Heddle.app is in /Applications, then refresh LaunchServices."
        case .devLocation(let path):
            return "This copy is running from \(path). Use the Finder install window to move Heddle.app to /Applications so macOS can register the embedded extension."
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

    /// Open a Finder-native install folder containing Heddle.app and
    /// an Applications shortcut. Finder owns the final drag, which avoids
    /// app-sandbox copy failures for /Applications.
    func openInstallShortcutWindow() {
        guard installState != .preparingShortcut else {
            return
        }

        installState = .preparingShortcut
        lastMessage = "Preparing a Finder install window..."

        Task {
            let result = await Self.prepareInstallShortcutWindow(
                sourceURL: Bundle.main.bundleURL.standardizedFileURL
            )

            switch result {
            case .success(let folder):
                installState = .shortcutReady(folder.path)
                lastMessage =
                    "Drag Heddle.app onto the Applications shortcut in the Finder install window."
                NSWorkspace.shared.open(folder)
            case .failure(let error):
                noteInstallFailure(
                    "Could not prepare the Finder install window: \(error.localizedDescription). " +
                    "Reveal the app in Finder and drag it to /Applications."
                )
                revealAppInFinder()
            }
        }
    }

    /// Copy this host app into /Applications. This makes the embedded
    /// ExtensionKit FSKit module discoverable by LaunchServices.
    func installToApplications(from sourceURL: URL? = nil) {
        guard installState != .copying else {
            return
        }

        let source = (sourceURL ?? Bundle.main.bundleURL).standardizedFileURL

        guard source.pathExtension == "app" else {
            noteInstallFailure("Drop the Heddle app here, not \(source.lastPathComponent).")
            return
        }

        if let bundleIdentifier = Bundle(url: source)?.bundleIdentifier,
           bundleIdentifier != Bundle.main.bundleIdentifier {
            noteInstallFailure("Drop the Heddle app here, not \(source.lastPathComponent).")
            return
        }

        installState = .copying
        lastMessage = "Copying \(Self.installAppName) to /Applications..."

        Task {
            let result = await Self.copyAppToApplications(sourceURL: source)

            switch result {
            case .success(let destination):
                installState = .installed(destination.path)
                lastMessage =
                    "Copied \(Self.installAppName) to /Applications. " +
                    "Open that copy so macOS can register the embedded FSKit extension."
                NSWorkspace.shared.activateFileViewerSelecting([destination])
                refresh()
            case .failure(let error):
                lastMessage =
                    "Could not copy automatically: \(error.localizedDescription). " +
                    "Opening a Finder install window instead."
                openInstallShortcutWindow()
            }
        }
    }

    func noteInstallFailure(_ message: String) {
        installState = .failed(message)
        lastMessage = message
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
                    installState.installStatusMessage ??
                    "Drag Heddle.app into /Applications, relaunch it from there, " +
                    "then open System Settings."
            } else {
                status = .unregistered
                lastMessage =
                    "pluginkit did not list \(Self.bundleIdentifier). Run lsregister for /Applications/Heddle.app if the app was just copied."
            }
        case .failed(let message):
            if isLikelyDevLocation(bundlePath) {
                status = .devLocation(bundlePath)
            } else {
                status = .unregistered
            }
            if Self.isUnauthorizedDiscoveryMessage(message) {
                lastMessage =
                    installState.installStatusMessage ??
                    "Install Heddle.app in /Applications, then open System Settings to confirm the File System Extensions row."
            } else {
                lastMessage = "Could not run pluginkit probe: \(message)"
            }
        }
    }

    private func settingsDestination() -> SettingsDestination {
        let version = ProcessInfo.processInfo.operatingSystemVersion

        if #available(macOS 26, *) {
            // macOS 26 Tahoe: `?Extensions` lands on Login Items &
            // Extensions, which is as deep as the URL scheme goes —
            // empirically there is no public anchor that opens the
            // File System Extensions sheet directly, and the
            // Category-vs-By-App choice is a grouping control *inside*
            // that sheet rather than a separate destination. So we land
            // here and rely on the onboarding copy to tell the user to
            // group by Category (the By App toggle no-ops; Apple bug).
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

    private nonisolated static func copyAppToApplications(sourceURL: URL) async -> Result<URL, Error> {
        await Task.detached(priority: .userInitiated) {
            let fileManager = FileManager.default
            let applicationsURL = URL(fileURLWithPath: "/Applications", isDirectory: true)
            let destinationURL = applicationsURL.appendingPathComponent(
                installAppName,
                isDirectory: true
            )

            do {
                let sourcePath = sourceURL.resolvingSymlinksInPath().path
                let destinationPath = destinationURL.resolvingSymlinksInPath().path

                if sourcePath == destinationPath {
                    return .success(destinationURL)
                }

                if fileManager.fileExists(atPath: destinationURL.path) {
                    try fileManager.removeItem(at: destinationURL)
                }

                try fileManager.copyItem(at: sourceURL, to: destinationURL)
                return .success(destinationURL)
            } catch {
                return .failure(error)
            }
        }.value
    }

    private nonisolated static func prepareInstallShortcutWindow(sourceURL: URL) async -> Result<URL, Error> {
        await Task.detached(priority: .userInitiated) {
            let fileManager = FileManager.default
            let installFolder = fileManager.temporaryDirectory
                .appendingPathComponent("Heddle Install", isDirectory: true)
            let stagedApp = installFolder.appendingPathComponent(
                installAppName,
                isDirectory: true
            )
            let applicationsShortcut = installFolder.appendingPathComponent(
                "Applications",
                isDirectory: true
            )
            let applicationsURL = URL(fileURLWithPath: "/Applications", isDirectory: true)

            do {
                if fileManager.fileExists(atPath: installFolder.path) {
                    try fileManager.removeItem(at: installFolder)
                }

                try fileManager.createDirectory(
                    at: installFolder,
                    withIntermediateDirectories: true
                )
                try fileManager.copyItem(at: sourceURL, to: stagedApp)
                try fileManager.createSymbolicLink(
                    at: applicationsShortcut,
                    withDestinationURL: applicationsURL
                )

                return .success(installFolder)
            } catch {
                return .failure(error)
            }
        }.value
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

    private nonisolated static func isUnauthorizedDiscoveryMessage(_ message: String) -> Bool {
        message.localizedCaseInsensitiveContains("unauthorized discovery flag") ||
            message.localizedCaseInsensitiveContains("PKDiscoverAll")
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

private extension ExtensionManager.InstallState {
    var installStatusMessage: String? {
        switch self {
        case .idle:
            return nil
        case .copying:
            return "Copying Heddle.app to /Applications..."
        case .preparingShortcut:
            return "Preparing a Finder install window..."
        case .shortcutReady:
            return "Drag Heddle.app onto the Applications shortcut in the Finder install window."
        case .installed(let path):
            return "Copied Heddle.app to \(path). Open that copy so macOS can register the embedded FSKit extension."
        case .failed(let message):
            return message
        }
    }
}

private enum PluginKitProbe: Sendable {
    case enabled
    case disabled
    case absent
    case failed(String)
}
