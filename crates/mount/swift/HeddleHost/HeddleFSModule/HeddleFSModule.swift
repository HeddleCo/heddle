// SPDX-License-Identifier: Apache-2.0
//
// FSKit System Extension entry point. Implements the
// `UnaryFileSystemExtension` protocol; macOS instantiates this
// struct once per mount request and asks `fileSystem` for the
// `FSUnaryFileSystem` instance that handles it.

import ExtensionFoundation
import FSKit
import Foundation

@main
struct HeddleFSModule: UnaryFileSystemExtension {
    var fileSystem: FSUnaryFileSystem & FSUnaryFileSystemOperations {
        HeddleFSModuleFileSystem()
    }
}
