// SPDX-License-Identifier: Apache-2.0
//
// `FSVolume` subclass plus conformance to the FSKit volume
// operation protocols. Each protocol method translates between
// FSKit's argument shape and the Rust C ABI exposed through
// `HeddleSession`, then translates the response back.
//
// The Rust side is read-write but with a narrow surface: lookup,
// getattr, read, enumerate, write, flush. The other protocol
// methods (create, mkdir, remove, rename, setattr) have no
// matching `PlatformShell` hook today — they return
// `POSIXError(.ENOTSUP)` or `.EROFS` so the kernel returns a
// sensible errno to userspace instead of crashing.
//
// Inode-space translation
// -----------------------
//   FSKit reserves the `FSItem.Identifier` values 0 (invalid) and
//   1 (parent-of-root). Our Rust core uses NodeId 1 for the root.
//   We bridge with a +1 offset on non-root items, and special-case
//   root ↔ `FSItem.Identifier.rootDirectory` (2).

import Foundation
import FSKit
import OSLog

private let log = Logger(
    subsystem: "sh.heddle.HeddleFSModule",
    category: "HeddleVolume"
)

/// Instruments signpost emitter. Each FSKit hot op (`lookupItem`,
/// `getAttributes`, `read`, `enumerateDirectory`) brackets its
/// Swift body + Rust dispatch in a signpost interval, so Instruments
/// shows per-op latency in a swimlane and the difference between
/// "Swift took N µs" and "Rust took M µs" is visible without an
/// attached debugger.
///
/// Profile with: `xcrun xctrace record --template 'Time Profiler'
/// --launch HeddleHost` and add a signpost lane filtered to the
/// `sh.heddle.HeddleFSModule` subsystem.
@available(macOS 12.0, *)
private let signposter = OSSignposter(
    subsystem: "sh.heddle.HeddleFSModule",
    category: "FSOps"
)

/// Our convention: Rust `NodeId::ROOT == 1`.
let heddleRootInode: UInt64 = 1

// MARK: - HeddleVolume ------------------------------------------

final class HeddleVolume: FSVolume {
    let session: HeddleSession
    private let rootItem: HeddleItem
    /// Held so the security-scoped grant on the repo URL stays
    /// active for the lifetime of the mount. Released in deinit.
    /// `nil` when the URL wasn't scoped (e.g. caller path).
    private let securityScopedURL: URL?

    init(
        session: HeddleSession,
        volumeID: FSVolume.Identifier,
        volumeName: FSFileName,
        securityScopedURL: URL?
    ) {
        self.session = session
        self.securityScopedURL = securityScopedURL
        let rootAttrs = HeddleVolume.fetchAttrs(
            session: session, inode: heddleRootInode
        )
        self.rootItem = HeddleItem(
            inode: heddleRootInode,
            kind: .directory,
            size: rootAttrs?.size ?? 0,
            unixMode: rootAttrs?.unixMode ?? 0o040755
        )
        super.init(volumeID: volumeID, volumeName: volumeName)
    }

    deinit {
        securityScopedURL?.stopAccessingSecurityScopedResource()
    }

    /// Helper: fetch attrs for an inode via the Rust getattr
    /// callback. Returns nil on any failure. `mtimeSec` is
    /// seconds-since-UNIX-epoch (matches the Rust C ABI).
    fileprivate static func fetchAttrs(
        session: HeddleSession, inode: UInt64
    ) -> (unixMode: UInt32, size: UInt64, nlink: UInt32, mtimeSec: Int64)? {
        guard let getattr = session.session.getattr else { return nil }
        var unixMode: UInt32 = 0
        var size: UInt64 = 0
        var nlink: UInt32 = 0
        var mtimeSec: Int64 = 0
        let rc = getattr(session.session.userData, inode, &unixMode, &size, &nlink, &mtimeSec)
        guard rc == 0 else { return nil }
        return (unixMode, size, nlink, mtimeSec)
    }
}

// MARK: - Item ID <-> Node ID helpers ---------------------------

private func itemID(forNodeID nodeID: UInt64) -> FSItem.Identifier {
    if nodeID == heddleRootInode {
        return .rootDirectory
    }
    return FSItem.Identifier(rawValue: nodeID + 1) ?? .invalid
}

private func nodeID(forItem item: FSItem) -> UInt64? {
    guard let heddle = item as? HeddleItem else { return nil }
    return heddle.inode
}

private func itemType(fromUnixMode mode: UInt32) -> FSItem.ItemType {
    switch mode & 0o170000 {
    case 0o040000: return .directory
    case 0o120000: return .symlink
    default:       return .file
    }
}

private func heddleKind(fromUnixMode mode: UInt32) -> HeddleItemKind {
    switch mode & 0o170000 {
    case 0o040000: return .directory
    case 0o120000: return .symlink
    default:       return .file
    }
}

private func populateAttributes(
    _ attrs: FSItem.Attributes,
    inode: UInt64,
    unixMode: UInt32,
    size: UInt64,
    nlink: UInt32,
    mtimeSec: Int64
) {
    attrs.fileID = itemID(forNodeID: inode)
    attrs.type = itemType(fromUnixMode: unixMode)
    attrs.size = size
    attrs.allocSize = size
    attrs.linkCount = nlink
    attrs.mode = unixMode & 0o7777
    attrs.uid = 0
    attrs.gid = 0
    attrs.flags = 0
    var ts = timespec()
    ts.tv_sec = Int(mtimeSec)
    ts.tv_nsec = 0
    // The mount has no per-blob clock: change/access/birth all
    // share the bootstrap time. Better than Jan-1-1970 nonsense
    // in `ls -l`.
    attrs.modifyTime = ts
    attrs.changeTime = ts
    attrs.accessTime = ts
    attrs.birthTime = ts
}

// MARK: - FSVolume.PathConfOperations ---------------------------

extension HeddleVolume: FSVolume.PathConfOperations {
    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
}

// MARK: - FSVolume.Operations -----------------------------------

extension HeddleVolume: FSVolume.Operations {
    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsHardLinks = false
        caps.supportsSymbolicLinks = true
        caps.supportsPersistentObjectIDs = true
        caps.doesNotSupportVolumeSizes = true
        return caps
    }

    var volumeStatistics: FSStatFSResult {
        let result = FSStatFSResult(fileSystemTypeName: "heddle")
        result.blockSize = 4096
        result.ioSize = 64 * 1024
        return result
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping (Error?) -> Void) {
        log.info("HeddleVolume mount")
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        log.info("HeddleVolume unmount")
        replyHandler()
    }

    func synchronize(flags: FSSyncFlags, replyHandler: @escaping (Error?) -> Void) {
        replyHandler(nil)
    }

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest,
        of item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, Error?) -> Void
    ) {
        let interval = signposter.beginInterval("getAttributes")
        defer { signposter.endInterval("getAttributes", interval) }

        guard let inode = nodeID(forItem: item) else {
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        guard let res = HeddleVolume.fetchAttrs(session: session, inode: inode) else {
            replyHandler(nil, POSIXError(.EIO))
            return
        }
        let attrs = FSItem.Attributes()
        populateAttributes(
            attrs,
            inode: inode,
            unixMode: res.unixMode,
            size: res.size,
            nlink: res.nlink,
            mtimeSec: res.mtimeSec
        )
        replyHandler(attrs, nil)
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest,
        on item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, Error?) -> Void
    ) {
        // Accept-and-no-op: editors (vim) call setattr with
        // size=0 before writing; refusing here would break them.
        getAttributes(
            FSItem.GetAttributesRequest(),
            of: item,
            replyHandler: replyHandler
        )
    }

    func lookupItem(
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSItem?, FSFileName?, Error?) -> Void
    ) {
        let interval = signposter.beginInterval("lookupItem")
        defer { signposter.endInterval("lookupItem", interval) }

        guard let parentInode = nodeID(forItem: directory) else {
            replyHandler(nil, nil, POSIXError(.EINVAL))
            return
        }
        guard let lookup = session.session.lookup else {
            replyHandler(nil, nil, POSIXError(.ENOTSUP))
            return
        }
        let nameStr = name.string ?? ""
        var childInode: UInt64 = 0
        var unixMode: UInt32 = 0
        var size: UInt64 = 0
        let rc = nameStr.withCString { cstr in
            lookup(session.session.userData, parentInode, cstr,
                   &childInode, &unixMode, &size)
        }
        switch rc {
        case 0:
            let item = HeddleItem(
                inode: childInode,
                kind: heddleKind(fromUnixMode: unixMode),
                size: size,
                unixMode: unixMode
            )
            replyHandler(item, name, nil)
        case Int32(ENOENT):
            replyHandler(nil, nil, POSIXError(.ENOENT))
        default:
            replyHandler(nil, nil, POSIXError(.init(rawValue: rc) ?? .EIO))
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler: @escaping (Error?) -> Void) {
        replyHandler(nil)
    }

    func readSymbolicLink(
        _ item: FSItem,
        replyHandler: @escaping (FSFileName?, Error?) -> Void
    ) {
        replyHandler(nil, POSIXError(.ENOTSUP))
    }

    func createItem(
        named name: FSFileName,
        type: FSItem.ItemType,
        inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        replyHandler: @escaping (FSItem?, FSFileName?, Error?) -> Void
    ) {
        replyHandler(nil, nil, POSIXError(.EROFS))
    }

    func createSymbolicLink(
        named name: FSFileName,
        inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName,
        replyHandler: @escaping (FSItem?, FSFileName?, Error?) -> Void
    ) {
        replyHandler(nil, nil, POSIXError(.EROFS))
    }

    func createLink(
        to item: FSItem,
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSFileName?, Error?) -> Void
    ) {
        replyHandler(nil, POSIXError(.EROFS))
    }

    func removeItem(
        _ item: FSItem,
        named name: FSFileName,
        fromDirectory directory: FSItem,
        replyHandler: @escaping (Error?) -> Void
    ) {
        replyHandler(POSIXError(.EROFS))
    }

    func renameItem(
        _ item: FSItem,
        inDirectory sourceDirectory: FSItem,
        named sourceName: FSFileName,
        to destinationName: FSFileName,
        inDirectory destinationDirectory: FSItem,
        overItem: FSItem?,
        replyHandler: @escaping (FSFileName?, Error?) -> Void
    ) {
        replyHandler(nil, POSIXError(.EROFS))
    }

    func enumerateDirectory(
        _ directory: FSItem,
        startingAt cookie: FSDirectoryCookie,
        verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?,
        packer: FSDirectoryEntryPacker,
        replyHandler: @escaping (FSDirectoryVerifier, Error?) -> Void
    ) {
        let interval = signposter.beginInterval("enumerateDirectory")
        defer { signposter.endInterval("enumerateDirectory", interval) }

        guard let dirInode = nodeID(forItem: directory) else {
            replyHandler(verifier, POSIXError(.EINVAL))
            return
        }
        guard let enumerate = session.session.enumerate else {
            replyHandler(verifier, POSIXError(.ENOTSUP))
            return
        }

        final class EnumBuffer {
            struct Entry {
                let inode: UInt64
                let name: String
                let unixMode: UInt32
                let size: UInt64
                let mtimeSec: Int64
            }
            var entries: [Entry] = []
        }
        let buffer = EnumBuffer()
        let bufferPtr = Unmanaged.passUnretained(buffer).toOpaque()

        let rc = enumerate(
            session.session.userData,
            dirInode,
            bufferPtr,
            { (userData, childInode, namePtr, unixMode, size, mtimeSec) -> Int32 in
                guard let userData = userData, let namePtr = namePtr else {
                    return Int32(EINVAL)
                }
                let buf = Unmanaged<EnumBuffer>
                    .fromOpaque(userData)
                    .takeUnretainedValue()
                let name = String(cString: namePtr)
                buf.entries.append(.init(
                    inode: childInode,
                    name: name,
                    unixMode: unixMode,
                    size: size,
                    mtimeSec: mtimeSec
                ))
                return 0
            }
        )
        if rc != 0 {
            replyHandler(verifier, POSIXError(.init(rawValue: rc) ?? .EIO))
            return
        }

        let startIndex = Int(cookie.rawValue)
        for (i, entry) in buffer.entries.enumerated() where i >= startIndex {
            let type = itemType(fromUnixMode: entry.unixMode)
            let id = itemID(forNodeID: entry.inode)
            let entryAttrs: FSItem.Attributes?
            if attributes != nil {
                let a = FSItem.Attributes()
                populateAttributes(
                    a,
                    inode: entry.inode,
                    unixMode: entry.unixMode,
                    size: entry.size,
                    nlink: 1,
                    mtimeSec: entry.mtimeSec
                )
                entryAttrs = a
            } else {
                entryAttrs = nil
            }
            let nextCookie = FSDirectoryCookie(rawValue: UInt64(i + 1))
            let ok = packer.packEntry(
                name: FSFileName(string: entry.name),
                itemType: type,
                itemID: id,
                nextCookie: nextCookie,
                attributes: entryAttrs
            )
            if !ok { break }
        }
        replyHandler(FSDirectoryVerifier(rawValue: 1), nil)
    }

    func activate(
        options: FSTaskOptions,
        replyHandler: @escaping (FSItem?, Error?) -> Void
    ) {
        log.info("HeddleVolume activate")
        replyHandler(rootItem, nil)
    }

    func deactivate(
        options: FSDeactivateOptions,
        replyHandler: @escaping (Error?) -> Void
    ) {
        log.info("HeddleVolume deactivate")
        replyHandler(nil)
    }
}

// MARK: - FSVolume.OpenCloseOperations --------------------------

extension HeddleVolume: FSVolume.OpenCloseOperations {
    func openItem(
        _ item: FSItem,
        modes: FSVolume.OpenModes,
        replyHandler: @escaping (Error?) -> Void
    ) {
        replyHandler(nil)
    }

    func closeItem(
        _ item: FSItem,
        modes: FSVolume.OpenModes,
        replyHandler: @escaping (Error?) -> Void
    ) {
        guard let inode = nodeID(forItem: item) else {
            replyHandler(nil)
            return
        }
        if let flush = session.session.flush {
            let rc = flush(session.session.userData, inode)
            if rc != 0 {
                log.warning("closeItem: flush returned \(rc) for inode \(inode)")
            }
        }
        replyHandler(nil)
    }
}

// MARK: - FSVolume.ReadWriteOperations --------------------------

extension HeddleVolume: FSVolume.ReadWriteOperations {
    func read(
        from item: FSItem,
        at offset: off_t,
        length: Int,
        into buffer: FSMutableFileDataBuffer,
        replyHandler: @escaping (Int, Error?) -> Void
    ) {
        let interval = signposter.beginInterval("read")
        defer { signposter.endInterval("read", interval) }

        guard let inode = nodeID(forItem: item) else {
            replyHandler(0, POSIXError(.EINVAL))
            return
        }
        guard let read = session.session.read else {
            replyHandler(0, POSIXError(.ENOTSUP))
            return
        }
        var bytesRead: UInt64 = 0
        let rc = buffer.withUnsafeMutableBytes { raw -> Int32 in
            let base = raw.bindMemory(to: UInt8.self).baseAddress
            let cap = UInt64(min(length, raw.count))
            return read(
                session.session.userData,
                inode,
                UInt64(offset),
                base,
                cap,
                &bytesRead
            )
        }
        if rc != 0 {
            replyHandler(0, POSIXError(.init(rawValue: rc) ?? .EIO))
            return
        }
        replyHandler(Int(bytesRead), nil)
    }

    func write(
        contents: Data,
        to item: FSItem,
        at offset: off_t,
        replyHandler: @escaping (Int, Error?) -> Void
    ) {
        guard let inode = nodeID(forItem: item) else {
            replyHandler(0, POSIXError(.EINVAL))
            return
        }
        guard let write = session.session.write else {
            replyHandler(0, POSIXError(.EROFS))
            return
        }
        var bytesWritten: UInt64 = 0
        let rc = contents.withUnsafeBytes { raw -> Int32 in
            let base = raw.bindMemory(to: UInt8.self).baseAddress
            return write(
                session.session.userData,
                inode,
                UInt64(offset),
                base,
                UInt64(raw.count),
                &bytesWritten
            )
        }
        if rc != 0 {
            replyHandler(0, POSIXError(.init(rawValue: rc) ?? .EIO))
            return
        }
        replyHandler(Int(bytesWritten), nil)
    }
}
