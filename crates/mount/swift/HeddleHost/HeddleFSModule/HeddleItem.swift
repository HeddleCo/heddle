// SPDX-License-Identifier: Apache-2.0
//
// `FSItem` subclass — one instance per known inode. Mirrors the
// `NodeId` returned from `PlatformShell::lookup` /
// `PlatformShell::enumerate` on the Rust side.
//
// Attributes are cached at construction time so `getAttributes`
// is a hash-table hit rather than a re-walk; the Rust core's
// `attrs()` method is cheap but FSKit calls it often.

import Foundation
import FSKit

enum HeddleItemKind {
    case directory
    case file
    case symlink
}

final class HeddleItem: FSItem {
    let inode: UInt64
    let kind: HeddleItemKind
    let size: UInt64
    let unixMode: UInt32

    init(inode: UInt64, kind: HeddleItemKind, size: UInt64, unixMode: UInt32) {
        self.inode = inode
        self.kind = kind
        self.size = size
        self.unixMode = unixMode
        super.init()
    }
}
