// SPDX-License-Identifier: Apache-2.0
//
// Owns the C ABI handle into the Heddle Rust core.
//
// The handle is produced by `heddle_fskit_open_thread` (declared
// in `HeddleFSKit-Bridging.h` and implemented in the Rust crate
// at `crates/mount/src/fskit/c_abi.rs`). It points to a
// `HeddleFSKitSession` instance (the in-process Swift class
// defined in `HeddleFSKit.swift`) which holds the seven C
// callbacks Rust registered for us to call back into.
//
// In other words: Rust hands us a session object that *we* can use
// to dispatch syscalls (lookup, read, …) back into Rust. Each
// dispatch path on `HeddleVolume` reaches into `self.session` to
// pull the appropriate callback function pointer and invoke it.

import Foundation
import OSLog

private let log = Logger(
    subsystem: "sh.heddle.HeddleFSModule",
    category: "HeddleSession"
)

/// Wraps a live `HeddleFSKitSessionHandle` and the
/// `HeddleFSKitSession` it owns. Dropping this object releases
/// both (via `heddle_fskit_session_free`).
final class HeddleSession {
    /// Raw handle Rust returned from `heddle_fskit_open_thread`.
    /// Stored as the opaque pointer; resolved to a
    /// `HeddleFSKitSession` lazily via `session`.
    let handle: HeddleFSKitSessionHandle

    /// The Swift `HeddleFSKitSession` instance behind `handle`.
    /// Caches it so we don't redo the `Unmanaged.fromOpaque` round
    /// trip on every syscall.
    let session: HeddleFSKitSession

    private init(handle: HeddleFSKitSessionHandle, session: HeddleFSKitSession) {
        self.handle = handle
        self.session = session
    }

    /// Open a session for the given repo path + thread name.
    ///
    /// Returns `nil` if the Rust side couldn't open the repo or
    /// resolve the thread. Diagnostic detail is logged through
    /// `OSLog` because System Extension stdout isn't visible.
    static func open(repoPath: String, threadID: String) -> HeddleSession? {
        let handle: HeddleFSKitSessionHandle? = repoPath.withCString { repoCStr in
            threadID.withCString { threadCStr in
                heddle_fskit_open_thread(repoCStr, threadCStr)
            }
        }
        guard let handle else {
            log.error(
                "heddle_fskit_open_thread returned null for repo=\(repoPath, privacy: .public) thread=\(threadID, privacy: .public)"
            )
            return nil
        }
        let session = Unmanaged<HeddleFSKitSession>
            .fromOpaque(handle)
            .takeUnretainedValue()
        return HeddleSession(handle: handle, session: session)
    }

    deinit {
        heddle_fskit_session_free(handle)
    }
}
