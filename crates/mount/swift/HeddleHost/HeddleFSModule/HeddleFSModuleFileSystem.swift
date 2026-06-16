// SPDX-License-Identifier: Apache-2.0
//
// `FSUnaryFileSystem` implementation. FSKit calls these methods on
// the extension when the kernel wants to probe or activate a
// resource (i.e. mount a volume of our type).
//
// The real read/write/lookup/enumerate logic lives on
// `HeddleVolume`; this class is the top-level entry point FSKit
// binds to.
//
// How the repo path + thread name reach the extension
// ---------------------------------------------------
//
// FSKit invokes `loadResource` with:
//   * `resource: FSResource` — typically `FSPathURLResource`
//     wrapping the path the user gave `mount -F -t heddle <path>`.
//   * `options: FSTaskOptions` — the `-o key=value,…` options
//     declared by `FSActivateOptionSyntax` in Info.plist.
//
// For Heddle the convention is:
//   * resource path → the heddle repository root
//   * `-o t=<thread>` → which thread to mount (defaults to "main")
//
// `threadID(from:)` below parses the argv shape FSKit hands us
// based on `FSActivateOptionSyntax.shortOptions = "o:"` in
// Info.plist. The `crates/cli/src/cli/commands/mount_lifecycle.rs`
// macOS path issues `mount -F -t heddle -o t=<thread> ...`;
// mount(8) forwards the whole `-o` payload into FSKit task options.

import CryptoKit
import Foundation
import FSKit
import OSLog

private let log = Logger(
    subsystem: "sh.heddle.HeddleFSModule",
    category: "HeddleFSModuleFileSystem"
)

@objc(HeddleFSModuleFileSystem)
final class HeddleFSModuleFileSystem: FSUnaryFileSystem,
                                      FSUnaryFileSystemOperations {

    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        // Heddle volumes are virtual; there's no on-disk signature
        // to probe. The critical bit: FSKit (unary FS contract)
        // requires the container UUID returned here to MATCH the
        // volume UUID returned by `loadResource`. We derive both
        // from the resource path so they're stable.
        guard let resourceKey = HeddleFSModuleFileSystem.resourceKey(for: resource) else {
            log.error("probeResource: unsupported FSResource")
            replyHandler(.notRecognized, nil)
            return
        }

        let id = HeddleFSModuleFileSystem.stableUUID(forResourceKey: resourceKey)
        replyHandler(
            .usable(
                name: "heddle",
                containerID: FSContainerIdentifier(uuid: id)
            ),
            nil
        )
    }

    /// Derives a deterministic UUID from the resource so `probeResource`
    /// and `loadResource` return matching container/volume IDs for
    /// the same `mount(8)` invocation. SHA-256 of the path, first 16
    /// bytes treated as a UUID. Two different repos get different
    /// UUIDs; the same repo opened twice gets the same UUID.
    static func stableUUID(for resource: FSResource) -> UUID {
        stableUUID(forResourceKey: resourceKey(for: resource) ?? "heddle://unsupported")
    }

    private static func stableUUID(forResourceKey key: String) -> UUID {
        let digest = SHA256.hash(data: Data(key.utf8))
        let bytes = Array(digest.prefix(16))
        return UUID(uuid: (
            bytes[0], bytes[1], bytes[2], bytes[3],
            bytes[4], bytes[5], bytes[6], bytes[7],
            bytes[8], bytes[9], bytes[10], bytes[11],
            bytes[12], bytes[13], bytes[14], bytes[15]
        ))
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        // Pull the URL and keep the URL handle around, not just the
        // path string, so any security-scoped grant FSKit attaches to
        // the resource can stay active for the mount lifetime.
        guard let resourceURL = Self.repoURL(from: resource) else {
            log.error("loadResource: resource is not a file URL")
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        let repoPath = resourceURL.path
        let threadID = HeddleFSModuleFileSystem.threadID(from: options) ?? "main"

        log.info("loadResource: repo=\(repoPath, privacy: .public) thread=\(threadID, privacy: .public)")

        // Activate the security-scoped grant. This is process-wide,
        // so the Rust `std::fs` calls inside `heddle_fskit_open_thread`
        // inherit access to anything under `resourceURL`.
        let granted = resourceURL.startAccessingSecurityScopedResource()
        log.info("startAccessingSecurityScopedResource granted=\(granted, privacy: .public)")

        // Swift-side sandbox probe: try to read the path's metadata
        // via FileManager. If Swift can do this but Rust can't, the
        // issue is somewhere in Rust. If Swift also fails, the
        // sandbox is the blocker.
        let fm = FileManager.default
        let exists = fm.fileExists(atPath: repoPath)
        let heddleDirExists = fm.fileExists(atPath: "\(repoPath)/.heddle")
        log.info("swift-probe repoPath exists=\(exists, privacy: .public) heddleDir exists=\(heddleDirExists, privacy: .public)")
        if let entries = try? fm.contentsOfDirectory(atPath: repoPath) {
            log.info("swift-probe contentsOfDirectory: \(entries.count, privacy: .public) entries: \(entries.joined(separator: ","), privacy: .public)")
        } else {
            log.error("swift-probe contentsOfDirectory FAILED — sandbox is blocking access")
        }

        guard let session = HeddleSession.open(
            repoPath: repoPath, threadID: threadID
        ) else {
            // Drop the grant before bailing.
            if granted {
                resourceURL.stopAccessingSecurityScopedResource()
            }
            replyHandler(nil, POSIXError(.ENOENT))
            return
        }

        log.info("constructing HeddleVolume")
        let stableID = HeddleFSModuleFileSystem.stableUUID(for: resource)
        let volume = HeddleVolume(
            session: session,
            volumeID: .init(uuid: stableID),
            volumeName: FSFileName(string: "heddle"),
            securityScopedURL: granted ? resourceURL : nil
        )

        // Transition the unary-FS container state out of .notReady
        // (default) to .ready before returning. Per FSKit docs:
        // loadResource → .ready; volume activation later → .active.
        self.containerStatus = FSContainerStatus.ready
        log.info("set containerStatus = ready")

        log.info("loadResource: replying with volume")
        replyHandler(volume, nil)
        log.info("loadResource: returned after replyHandler")
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions
    ) async throws {
        // Volume tear-down lands inside HeddleVolume / HeddleSession
        // deinits. Nothing extension-scoped to clean up here.
    }

    /// Parse the thread name out of `mount -F -t heddle -o t=<thread> …`.
    ///
    /// FSKit receives the mount(8) option payload as `-o`, so the normal
    /// CLI shape is one of:
    ///   * `["-o", "t=foo"]`
    ///   * `["-o=t=foo"]`
    ///   * `["-ot=foo"]`
    /// The parser also accepts old direct `-t` forms for compatibility.
    /// Returns nil if no thread option is present so the caller can default.
    static func threadID(from options: FSTaskOptions) -> String? {
        let argv = options.taskOptions
        var i = 0
        while i < argv.count {
            let arg = argv[i]
            if arg == "-o" {
                if i + 1 < argv.count, let thread = threadID(fromMountOptions: argv[i + 1]) {
                    return thread
                }
                i += 2
                continue
            }
            if arg.hasPrefix("-o=") {
                if let thread = threadID(fromMountOptions: String(arg.dropFirst(3))) {
                    return thread
                }
            }
            if arg.hasPrefix("-o") && arg.count > 2 {
                if let thread = threadID(fromMountOptions: String(arg.dropFirst(2))) {
                    return thread
                }
            }
            if arg == "-t" {
                if i + 1 < argv.count {
                    let value = argv[i + 1]
                    if !value.isEmpty { return value }
                }
                return nil
            }
            if arg.hasPrefix("-t=") {
                return String(arg.dropFirst(3))
            }
            if arg.hasPrefix("-t") && arg.count > 2 {
                return String(arg.dropFirst(2))
            }
            i += 1
        }
        return nil
    }

    private static func threadID(fromMountOptions raw: String) -> String? {
        for option in raw.split(separator: ",") {
            let trimmed = option.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty else { continue }
            let parts = trimmed.split(separator: "=", maxSplits: 1)
            guard parts.count == 2 else { continue }
            let key = parts[0].trimmingCharacters(in: .whitespacesAndNewlines)
            let value = parts[1].trimmingCharacters(in: .whitespacesAndNewlines)
            if (key == "t" || key == "thread") && !value.isEmpty {
                return value
            }
        }
        return nil
    }

    /// Extract a stable resource key from path-flavoured resources.
    /// Returns nil for block-device resources we don't support today.
    private static func resourceKey(for resource: FSResource) -> String? {
        if let pathURL = resource as? FSPathURLResource {
            return "heddle://path/\(pathURL.url.path)"
        }
        if let generic = resource as? FSGenericURLResource {
            return "heddle://url/\(generic.url.absoluteString)"
        }
        return nil
    }

    /// Extract a filesystem URL from any of the path-flavoured
    /// `FSResource` subclasses. Returns nil for block-device,
    /// non-file, or unsupported resources.
    private static func repoURL(from resource: FSResource) -> URL? {
        if let pathURL = resource as? FSPathURLResource {
            return pathURL.url
        }
        if let generic = resource as? FSGenericURLResource {
            let url = generic.url
            if url.isFileURL { return url }
        }
        return nil
    }
}
