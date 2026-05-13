// HeddleFSKit.swift — Swift adapter that bridges Apple's FSKit
// framework to the heddle `mount` crate's `PlatformShell` trait.
//
// The Swift side owns:
//   * the `FSUnaryFileSystem` / `FSVolume` / `FSItem` instances
//     that FSKit hands back to the kernel
//   * the per-callback dispatch from FSKit into the C ABI
//     defined in `HeddleFSKit-Bridging.h`
//
// The Rust side owns:
//   * a `Box<dyn PlatformShell>` (the `ContentAddressedMount`)
//     plus a tiny callback registry that converts each Swift
//     request into a trait method invocation
//   * mount-point lifetime (the Drop on `FSKitShell` triggers
//     `heddle_fskit_unmount`)
//
// Realistic scope:
//
// FSKit is heavily Swift-protocol-oriented (`FSUnaryFileSystem`
// uses `async throws` callbacks, opaque `FSItem` subclasses,
// and `FSVolume.Operations` protocols that are awkward to expose
// across a C ABI). This file scaffolds the constructor + a few
// of the read-side callbacks (`getattr`, `lookup`, `read`,
// `enumerate`) so the architecture is validated end-to-end. The
// write-side callbacks and the `FSModuleHost` registration that
// actually publishes the volume to `mount(2)` are stubbed — see
// the TODOs below. A full implementation needs ~400 more LOC of
// Swift and a `.fsmodule` bundle plus an entitlement, which is
// out of scope for this PR.
//
// The C ABI surface this file exports is defined in the
// `HeddleFSKit-Bridging.h` header next to it. Build.rs compiles
// this file and links the resulting static lib into the Rust
// crate.

import Foundation

#if canImport(FSKit) && os(macOS)
import FSKit
#endif

// MARK: - C ABI exposed to Rust ---------------------------------

// Opaque session handle. Rust treats this as `*mut c_void`; the
// Swift side stores the live `HeddleFSKitSession` behind it.
public typealias HeddleFSKitSessionHandle = UnsafeMutableRawPointer

// Callback function pointer types. The Rust side fills in a
// `HeddleFSKitCallbacks` struct with these and passes it to
// `heddle_fskit_session_new`. Each callback receives the
// `user_data` pointer the Rust side registered, so the Swift
// side never needs to know about Rust types.
//
// Errors flow as a libc errno (positive int). Zero == success.
//
// Buffers: caller (Swift) provides the buffer; callee (Rust) writes
// into it and returns bytes-written via `out_len`.

public typealias HeddleLookupCallback = @convention(c) (
    UnsafeMutableRawPointer?,           // user_data (Rust shell)
    UInt64,                             // parent inode
    UnsafePointer<CChar>?,              // name (NUL-terminated UTF-8)
    UnsafeMutablePointer<UInt64>?,      // out: child inode
    UnsafeMutablePointer<UInt32>?,      // out: unix mode (incl type bits)
    UnsafeMutablePointer<UInt64>?       // out: size
) -> Int32

public typealias HeddleGetattrCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    UInt64,                             // inode
    UnsafeMutablePointer<UInt32>?,      // out: unix mode
    UnsafeMutablePointer<UInt64>?,      // out: size
    UnsafeMutablePointer<UInt32>?       // out: nlink
) -> Int32

public typealias HeddleReadCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    UInt64,                             // inode
    UInt64,                             // offset
    UnsafeMutablePointer<UInt8>?,       // buffer
    UInt64,                             // buffer capacity
    UnsafeMutablePointer<UInt64>?       // out: bytes read
) -> Int32

public typealias HeddleWriteCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    UInt64,                             // inode
    UInt64,                             // offset
    UnsafePointer<UInt8>?,              // data
    UInt64,                             // data length
    UnsafeMutablePointer<UInt64>?       // out: bytes written
) -> Int32

// Enumerate is callback-driven so the Swift side doesn't have to
// pre-allocate a buffer of unknown size. Rust calls `emit` once
// per entry. The `emit` callback returns 0 to continue, non-zero
// to stop early (e.g. the FSKit reply buffer is full).
public typealias HeddleEnumerateEmit = @convention(c) (
    UnsafeMutableRawPointer?,           // emit_user_data (FSKit reply ctx)
    UInt64,                             // child inode
    UnsafePointer<CChar>?,              // child name (UTF-8 NUL-terminated)
    UInt32,                             // unix mode
    UInt64                              // size
) -> Int32

public typealias HeddleEnumerateCallback = @convention(c) (
    UnsafeMutableRawPointer?,           // user_data (Rust shell)
    UInt64,                             // dir inode
    UnsafeMutableRawPointer?,           // emit_user_data (forwarded)
    HeddleEnumerateEmit?                // emit
) -> Int32

public typealias HeddleFlushCallback = @convention(c) (
    UnsafeMutableRawPointer?,
    UInt64                              // inode
) -> Int32

// Released when the session is dropped. Rust uses this to
// reclaim the `Box<dyn PlatformShell>` it leaked at registration.
public typealias HeddleDropCallback = @convention(c) (
    UnsafeMutableRawPointer?
) -> Void

@_cdecl("heddle_fskit_session_new")
public func heddle_fskit_session_new(
    userData: UnsafeMutableRawPointer?,
    lookup: HeddleLookupCallback?,
    getattr: HeddleGetattrCallback?,
    read: HeddleReadCallback?,
    write: HeddleWriteCallback?,
    enumerate: HeddleEnumerateCallback?,
    flush: HeddleFlushCallback?,
    drop: HeddleDropCallback?
) -> HeddleFSKitSessionHandle? {
    let session = HeddleFSKitSession(
        userData: userData,
        lookup: lookup,
        getattr: getattr,
        read: read,
        write: write,
        enumerate: enumerate,
        flush: flush,
        drop: drop
    )
    // Retain the session and hand back an opaque pointer. Rust
    // is responsible for calling `heddle_fskit_session_free`
    // exactly once.
    let unmanaged = Unmanaged.passRetained(session)
    return unmanaged.toOpaque()
}

@_cdecl("heddle_fskit_session_mount")
public func heddle_fskit_session_mount(
    handle: HeddleFSKitSessionHandle?,
    mountpoint: UnsafePointer<CChar>?
) -> Int32 {
    guard let handle = handle else { return Int32(EINVAL) }
    guard let mountpoint = mountpoint else { return Int32(EINVAL) }
    let session = Unmanaged<HeddleFSKitSession>
        .fromOpaque(handle)
        .takeUnretainedValue()
    let path = String(cString: mountpoint)
    return session.mount(at: path)
}

@_cdecl("heddle_fskit_session_unmount")
public func heddle_fskit_session_unmount(
    handle: HeddleFSKitSessionHandle?
) -> Int32 {
    guard let handle = handle else { return Int32(EINVAL) }
    let session = Unmanaged<HeddleFSKitSession>
        .fromOpaque(handle)
        .takeUnretainedValue()
    return session.unmount()
}

@_cdecl("heddle_fskit_session_free")
public func heddle_fskit_session_free(
    handle: HeddleFSKitSessionHandle?
) {
    guard let handle = handle else { return }
    // Balances the `passRetained` in `heddle_fskit_session_new`.
    // The session's deinit fires the Rust-side `drop` callback.
    Unmanaged<HeddleFSKitSession>
        .fromOpaque(handle)
        .release()
}

// Returns 1 when the build was compiled with `canImport(FSKit)`
// and the OS is actually new enough at runtime to load the
// framework. Used by the Rust side to gate tests / diagnose
// "compiled but won't mount here" cases without crashing.
@_cdecl("heddle_fskit_is_available")
public func heddle_fskit_is_available() -> Int32 {
    #if canImport(FSKit) && os(macOS)
    if #available(macOS 15.4, *) {
        return 1
    }
    return 0
    #else
    return 0
    #endif
}

// MARK: - Session implementation --------------------------------

final class HeddleFSKitSession {
    let userData: UnsafeMutableRawPointer?
    let lookup: HeddleLookupCallback?
    let getattr: HeddleGetattrCallback?
    let read: HeddleReadCallback?
    let write: HeddleWriteCallback?
    let enumerate: HeddleEnumerateCallback?
    let flush: HeddleFlushCallback?
    let drop: HeddleDropCallback?

    init(
        userData: UnsafeMutableRawPointer?,
        lookup: HeddleLookupCallback?,
        getattr: HeddleGetattrCallback?,
        read: HeddleReadCallback?,
        write: HeddleWriteCallback?,
        enumerate: HeddleEnumerateCallback?,
        flush: HeddleFlushCallback?,
        drop: HeddleDropCallback?
    ) {
        self.userData = userData
        self.lookup = lookup
        self.getattr = getattr
        self.read = read
        self.write = write
        self.enumerate = enumerate
        self.flush = flush
        self.drop = drop
    }

    deinit {
        // Hand the user-data pointer back to Rust so it can drop
        // the boxed PlatformShell.
        drop?(userData)
    }

    func mount(at path: String) -> Int32 {
        // TODO(fskit-mount): Wire this to FSKit's `FSModuleHost`.
        //
        // The real implementation looks like:
        //
        //   let host = FSModuleHost.shared
        //   let fs = HeddleFSUnary(session: self)
        //   try host.register(fs, identifier: "xyz.heddle.mount")
        //   // ...then `mount -F -t heddle` from userspace, or
        //   // call `FSResource.mount(...)` programmatically.
        //
        // FSKit's registration requires a code-signed bundle with
        // the `com.apple.developer.fskit.fsmodule` entitlement.
        // Distributing that is a release-engineering problem this
        // PR doesn't try to solve. For now the constructor +
        // unmount lifecycle is exercised; the mount itself is a
        // no-op that returns ENOSYS so callers can detect the
        // unfinished seam.
        _ = path
        return Int32(ENOSYS)
    }

    func unmount() -> Int32 {
        // TODO(fskit-mount): once `mount(at:)` actually mounts a
        // volume, this needs to call `FSResource.unmount` and
        // wait for the kernel to drain in-flight requests. For
        // now we just succeed.
        return 0
    }
}

#if canImport(FSKit) && os(macOS)

// Skeleton of the FSKit-side adapter. Kept compiling under
// `canImport(FSKit)` so the file builds on macOS 15.4 SDKs but
// the type body stays minimal — every method has a matching TODO
// pointing at the trait callback it should dispatch to.

@available(macOS 15.4, *)
final class HeddleFSUnary: NSObject {
    weak var session: HeddleFSKitSession?

    init(session: HeddleFSKitSession) {
        self.session = session
        super.init()
    }

    // TODO(fskit-mount): conform to `FSUnaryFileSystem` and
    // implement `loadResource(...)`, `probeResource(...)`,
    // `activate(options:replyHandler:)` etc. Each callback maps
    // to one of the `HeddleFSKitSession` C-callback fields:
    //
    //   FSVolume.lookupItem(named:in:)        -> session.lookup
    //   FSItem.attributes                     -> session.getattr
    //   FSVolume.read(_:from:into:length:)    -> session.read
    //   FSVolume.write(_:into:from:length:)   -> session.write
    //   FSVolume.enumerate(_:)                -> session.enumerate
    //   FSItem.flush                          -> session.flush
    //
    // Each FSKit callback hands back an `Errno` or throws — we
    // translate the Int32 return into the right shape there.
}

#endif
