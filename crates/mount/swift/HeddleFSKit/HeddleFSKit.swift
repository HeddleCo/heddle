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
// SDK-verification result (plan §6.1):
//
// The macOS 26.5 SDK at
// `MacOSX.sdk/System/Library/Frameworks/FSKit.framework/Headers/`
// confirms the System-Extension model:
//
//   * `FSUnaryFileSystem` is real, with `probeResource:replyHandler:`
//     and `loadResource:options:replyHandler:` as the registration
//     surface (`FSUnaryFileSystem.h`).
//   * `FSVolume` is a subclass; per-operation behaviour comes from
//     conforming to the various `FSVolume.Operations` /
//     `FSVolume.ReadWriteOperations` / `FSVolume.OpenCloseOperations`
//     protocols (`FSVolume.h`).
//   * There is no `FSFileSystemManager` / programmatic in-process
//     `mount(at:)` API. Mounting is done out-of-process by the
//     `mount(8)` userspace tool against a registered `.fsmodule`
//     System Extension; the host app activates the extension via
//     the System Extensions framework.
//
// Consequence: the live FS code cannot run inside the `heddle` CLI
// process. It must run inside a code-signed `.fsmodule` System
// Extension bundle that links against this same static library.
//
// What this file ships today:
//   * The C ABI surface that Rust calls (untouched — required by
//     `crates/mount/src/fskit/c_abi.rs`).
//   * A `HeddleFSKitSession` class that holds the C-side callbacks
//     so any consumer (this CLI for the unit test, or a future
//     `.fsmodule` bundle) can dispatch into them.
//   * A minimal `HeddleFSUnary : FSUnaryFileSystem` placeholder that
//     compiles against the 15.4+ SDK so the `cargo build --features
//     fskit` cycle stays green.
//
// What's deferred to release engineering (out of scope here):
//   * `.fsmodule` / `.appex` System Extension bundle packaging.
//   * `OSSystemExtensionRequest.activationRequest(...)` from the
//     CLI to install the extension.
//   * `com.apple.developer.fskit.fsmodule` entitlement +
//     Developer-ID code-signing on both the CLI and the extension.
//   * Full conformance to `FSVolume.Operations` /
//     `FSVolume.ReadWriteOperations` (lookup, getAttributes,
//     read, write, enumerateDirectory, synchronize). Each protocol
//     method dispatches into the C callbacks held by
//     `HeddleFSKitSession` — the wiring pattern is the same as the
//     placeholder methods below.
//
// See `crates/mount/README.md` for the install steps that consume
// those artefacts.

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
    UnsafeMutablePointer<UInt32>?,      // out: nlink
    UnsafeMutablePointer<Int64>?        // out: mtime seconds since UNIX epoch
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
    UInt64,                             // size
    Int64                               // mtime seconds since UNIX epoch
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
        // FSKit on macOS 15.4+ has no programmatic in-process
        // `mount(at:)`. Mounting requires a code-signed `.fsmodule`
        // System Extension that registers via
        // `OSSystemExtensionRequest.activationRequest(...)` and is
        // then driven by `mount(8)` from userspace. The CLI process
        // cannot host the live FS.
        //
        // Returning ENOSYS keeps the existing contract intact: the
        // Rust side surfaces it as a `MountError`, the FSKit smoke
        // test treats `HEDDLE_FSKIT_AVAILABLE=1` as an entitlement
        // probe, and the CLI falls back through to its existing
        // error path.
        //
        // To finish the macOS path: package a `.fsmodule` bundle
        // that links this static library, request the FSKit
        // entitlement, and add `OSSystemExtensionRequest` activation
        // to the CLI. See `crates/mount/README.md`.
        _ = path
        return Int32(ENOSYS)
    }

    func unmount() -> Int32 {
        // Symmetric with `mount(at:)`: nothing was actually mounted
        // in-process, so unmount is a successful no-op.
        return 0
    }
}

#if canImport(FSKit) && os(macOS)

// MARK: - FSKit class skeletons ---------------------------------
//
// These compile against the real macOS 26.5 SDK headers. They are
// the entry points a future `.fsmodule` System Extension target
// would consume:
//
//   * `HeddleFSUnary` is the top-level filesystem class FSKit's
//     extension host instantiates. Per `FSUnaryFileSystem.h`, the
//     extension implements `probeResource:replyHandler:` and
//     `loadResource:options:replyHandler:`.
//   * `HeddleVolume` (subclass of `FSVolume`) is the per-volume
//     class that gets returned from `loadResource`. It must
//     conform to `FSVolume.Operations`,
//     `FSVolume.PathConfOperations`, and (for read-write mounts)
//     `FSVolume.ReadWriteOperations`. Each protocol method
//     dispatches into the C callbacks held by `HeddleFSKitSession`.
//   * `HeddleItem` (subclass of `FSItem`) wraps the `UInt64` inode
//     handed back from `lookup` / `enumerate`.
//
// Today these classes are present-but-empty so the cargo build
// stays green. Wiring each `FSVolume.Operations` method to the
// matching C callback is the next concrete task; the trampoline
// shape is the same as the Linux FUSE adapter in
// `crates/mount/src/fuse.rs`.

@available(macOS 15.4, *)
final class HeddleFSUnary: FSUnaryFileSystem {
    weak var session: HeddleFSKitSession?

    init(session: HeddleFSKitSession) {
        self.session = session
        super.init()
    }

    // Real `probeResource` / `loadResource` overrides land here
    // once the `.fsmodule` packaging story is in place. The
    // signatures live in
    // `FSKit.framework/Headers/FSUnaryFileSystem.h`.
}

// The canonical `HeddleItem` lives in the System Extension target
// (`crates/mount/swift/HeddleHost/HeddleFSModule/HeddleItem.swift`)
// where it's used alongside the full `FSVolume.Operations`
// conformance on `HeddleVolume`. The in-process (cargo-built)
// Swift static lib doesn't need its own copy because the
// in-process path never instantiates an `FSItem` — it dispatches
// trampolines directly into Rust without going through FSKit's
// item layer.

#endif
